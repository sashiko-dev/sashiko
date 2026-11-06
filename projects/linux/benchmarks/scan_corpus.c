// SPDX-License-Identifier: GPL-2.0
/*
 * Composite Linux kernel subsystem helpers for static & agentic analysis.
 */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
typedef int32_t s32;
typedef int64_t s64;
typedef unsigned int gfp_t;
typedef uint64_t dma_addr_t;
typedef uint64_t phys_addr_t;

#define EINVAL      22
#define ENOMEM      12
#define EIO          5
#define EAGAIN      11
#define EBUSY       16
#define ENODEV      19
#define EOPNOTSUPP  95
#define EFAULT      14

#define GFP_KERNEL  0x01u
#define GFP_ATOMIC  0x02u
#define ARRAY_SIZE(arr) (sizeof(arr) / sizeof((arr)[0]))
#define ALIGN(x, a) (((x) + ((a) - 1)) & ~((size_t)(a) - 1))
int kcorp_warn_on_impl(bool cond);
#define WARN_ON(cond) kcorp_warn_on_impl((bool)(cond))
#define container_of(ptr, type, member) \
    ((type *)((char *)(ptr) - offsetof(type, member)))

typedef struct { int locked; } spinlock_t;
struct mutex { int state; };
struct seqcount { unsigned int sequence; };
struct rcu_head { void (*func)(struct rcu_head *head); };
struct srcu_struct { int active; };
typedef struct { int refs; } refcount_t;
struct list_head { struct list_head *next, *prev; };
struct work_struct { void (*func)(struct work_struct *work); int pending; };
struct workqueue_struct { const char *name; int active_items; bool destroyed; };

void *kmalloc(size_t size, gfp_t flags);
void *kzalloc(size_t size, gfp_t flags);
void *kcalloc(size_t n, size_t size, gfp_t flags);
void kfree(const void *ptr);
void spin_lock(spinlock_t *lock);
void spin_unlock(spinlock_t *lock);
void spin_lock_irqsave(spinlock_t *lock, unsigned long *flags);
void spin_unlock_irqrestore(spinlock_t *lock, unsigned long flags);
void mutex_lock(struct mutex *lock);
void mutex_unlock(struct mutex *lock);
void rcu_read_lock(void);
void rcu_read_unlock(void);
bool rcu_read_lock_held(void);
void synchronize_rcu(void);
void call_rcu_free(struct rcu_head *head, size_t offset);
int srcu_read_lock(struct srcu_struct *sp);
void srcu_read_unlock(struct srcu_struct *sp, int idx);
void synchronize_srcu(struct srcu_struct *sp);
void preempt_disable(void);
void preempt_enable(void);
bool preemptible(void);
void cond_resched(void);
size_t copy_from_user(void *to, const void *from, size_t n);
size_t copy_to_user(void *to, const void *from, size_t n);
bool queue_work(struct workqueue_struct *wq, struct work_struct *work);
bool cancel_work_sync(struct work_struct *work);
void destroy_workqueue(struct workqueue_struct *wq);

/* --- NFC NCI controller & rfkill management --- */

struct kcorp_rfkill {
    void *priv;
    int (*set_block)(void *data, bool blocked);
    bool registered;
    bool hw_blocked;
};

struct kcorp_nci_dev {
    struct mutex req_lock;
    struct workqueue_struct *cmd_wq;
    struct work_struct close_work;
    struct kcorp_rfkill *rfk;
    u32 flags;
    bool is_open;
};

static inline void kcorp_rfkill_unregister(struct kcorp_rfkill *rfk)
{
    if (!rfk || !rfk->registered)
        return;
    /* Synchronously notify driver to block radio before tearing down rfkill. */
    if (rfk->set_block)
        rfk->set_block(rfk->priv, true);
    rfk->registered = false;
}

static inline void kcorp_rfkill_destroy(struct kcorp_rfkill *rfk)
{
    kfree(rfk);
}

static int kcorp_nci_close_device(struct kcorp_nci_dev *ndev)
{
    mutex_lock(&ndev->req_lock);
    if (!ndev->is_open) {
        mutex_unlock(&ndev->req_lock);
        return 0;
    }
    ndev->is_open = false;
    queue_work(ndev->cmd_wq, &ndev->close_work);
    mutex_unlock(&ndev->req_lock);
    return 0;
}

static int kcorp_nci_rfkill_set_block(void *data, bool blocked)
{
    struct kcorp_nci_dev *ndev = data;

    if (!blocked)
        return 0;
    return kcorp_nci_close_device(ndev);
}

int kcorp_nci_register_device(struct kcorp_nci_dev *ndev, struct workqueue_struct *wq)
{
    struct kcorp_rfkill *rfk;

    if (!ndev || !wq)
        return -EINVAL;

    rfk = kzalloc(sizeof(*rfk), GFP_KERNEL);
    if (!rfk)
        return -ENOMEM;

    rfk->priv = ndev;
    rfk->set_block = kcorp_nci_rfkill_set_block;
    rfk->registered = true;
    ndev->cmd_wq = wq;
    ndev->rfk = rfk;
    ndev->is_open = true;
    return 0;
}

/**
 * kcorp_nci_unregister_device - Unregister an NCI controller from subsystem
 * @ndev: NCI controller instance
 */
void kcorp_nci_unregister_device(struct kcorp_nci_dev *ndev)
{
    if (!ndev)
        return;

    cancel_work_sync(&ndev->close_work);
    if (ndev->cmd_wq) {
        destroy_workqueue(ndev->cmd_wq);
    }

    if (ndev->rfk) {
        kcorp_rfkill_unregister(ndev->rfk);
        kcorp_rfkill_destroy(ndev->rfk);
        ndev->rfk = NULL;
    }
}

/* --- Cgroup device memory (dmem) pool accounting --- */

struct kcorp_dmem_region {
    struct list_head pools;
    spinlock_t lock;
    u64 capacity_bytes;
    const char *name;
};

struct kcorp_dmem_pool {
    struct list_head region_node;
    struct kcorp_dmem_region *region;
    u64 charged_bytes;
    u64 max_bytes;
    unsigned int active_charges;
    bool unregistered;
};

struct kcorp_dmem_charge_handle {
    struct kcorp_dmem_pool *pool;
    u64 size;
};

static inline void kcorp_list_init(struct list_head *head)
{
    head->next = head;
    head->prev = head;
}

static inline void kcorp_list_add_tail(struct list_head *node, struct list_head *head)
{
    struct list_head *prev = head->prev;

    node->next = head;
    node->prev = prev;
    prev->next = node;
    head->prev = node;
}

static inline void kcorp_list_del(struct list_head *entry)
{
    entry->next->prev = entry->prev;
    entry->prev->next = entry->next;
    entry->next = NULL;
    entry->prev = NULL;
}

/**
 * kcorp_dmem_uncharge_pool - Release an active device memory charge
 * @handle: Charge handle previously returned by try_charge
 */
void kcorp_dmem_uncharge_pool(struct kcorp_dmem_charge_handle *handle)
{
    struct kcorp_dmem_pool *pool = handle->pool;
    struct kcorp_dmem_region *region = pool->region;
    unsigned long flags;

    spin_lock_irqsave(&region->lock, &flags);
    if (pool->charged_bytes >= handle->size)
        pool->charged_bytes -= handle->size;
    if (pool->active_charges > 0)
        pool->active_charges--;
    spin_unlock_irqrestore(&region->lock, flags);
    kfree(handle);
}

/**
 * kcorp_dmem_unregister_region - Detach a device memory region on driver unload
 * @region: Region being unregistered by backing driver
 */
void kcorp_dmem_unregister_region(struct kcorp_dmem_region *region)
{
    struct list_head *cur, *tmp;
    unsigned long flags;

    spin_lock_irqsave(&region->lock, &flags);
    cur = region->pools.next;
    while (cur != &region->pools) {
        struct kcorp_dmem_pool *pool =
            container_of(cur, struct kcorp_dmem_pool, region_node);
        tmp = cur->next;
        kcorp_list_del(&pool->region_node);
        pool->unregistered = true;
        kfree(pool);
        cur = tmp;
    }
    spin_unlock_irqrestore(&region->lock, flags);
}

/* --- GPU PCI bus binding inspection --- */

struct kcorp_pci_dev {
    refcount_t refcnt;
    u16 vendor_id;
    u16 device_id;
    u32 bdf;
    bool has_driver;
    char slot_name[32];
};

void kcorp_pci_dbg_emit(const char *slot, u32 bdf, bool bound);

static inline struct kcorp_pci_dev *kcorp_pci_get_domain_bus_and_slot(u32 bdf)
{
    struct kcorp_pci_dev *pdev = kzalloc(sizeof(*pdev), GFP_KERNEL);

    if (!pdev)
        return NULL;
    pdev->refcnt.refs = 1;
    pdev->bdf = bdf;
    return pdev;
}

static inline void kcorp_pci_dev_put(struct kcorp_pci_dev *pdev)
{
    if (!pdev)
        return;
    pdev->refcnt.refs--;
    if (pdev->refcnt.refs <= 0)
        kfree(pdev);
}

/**
 * kcorp_xe_pci_check_bound - Query whether sibling PCI function has a driver
 * @sibling_bdf: PCI BDF address to inspect
 */
bool kcorp_xe_pci_check_bound(u32 sibling_bdf)
{
    struct kcorp_pci_dev *pdev;
    bool bound = false;

    pdev = kcorp_pci_get_domain_bus_and_slot(sibling_bdf);
    if (!pdev)
        return false;

    bound = pdev->has_driver;
    kcorp_pci_dev_put(pdev);

    kcorp_pci_dbg_emit(pdev->slot_name, pdev->bdf, bound);
    return bound;
}

/* --- Ethernet bonding slave port management --- */

#define KCORP_BOND_MAX_SLAVES 16

struct kcorp_netdev {
    char name[16];
    u32 mtu;
    int (*ndo_open)(struct kcorp_netdev *dev);
    void (*ndo_stop)(struct kcorp_netdev *dev);
    int (*ndo_start_xmit)(struct kcorp_netdev *dev, const void *pkt, size_t len);
};

struct kcorp_bond_slave {
    struct kcorp_netdev *dev;
    u32 link_speed;
    bool active;
};

struct kcorp_bonding {
    spinlock_t slave_lock;
    struct kcorp_bond_slave *slaves[KCORP_BOND_MAX_SLAVES];
    unsigned int slave_cnt;
    unsigned int rr_tx_idx;
};

int kcorp_bond_xmit_roundrobin(struct kcorp_bonding *bond, const void *pkt, size_t len)
{
    struct kcorp_bond_slave *slave = NULL;
    unsigned int idx;

    spin_lock(&bond->slave_lock);
    if (bond->slave_cnt > 0) {
        idx = bond->rr_tx_idx++ % bond->slave_cnt;
        slave = bond->slaves[idx];
    }
    spin_unlock(&bond->slave_lock);

    if (!slave || !slave->active)
        return -ENODEV;
    return slave->dev->ndo_start_xmit(slave->dev, pkt, len);
}

/**
 * kcorp_bond_enslave_port - Attach a subordinate netdev to a bond aggregator
 * @bond: Bonding master context
 * @slave_dev: Subordinate netdev to enslave
 */
int kcorp_bond_enslave_port(struct kcorp_bonding *bond, struct kcorp_netdev *slave_dev)
{
    struct kcorp_bond_slave *new_slave;
    int err;

    if (!bond || !slave_dev || !slave_dev->ndo_open)
        return -EINVAL;

    new_slave = kzalloc(sizeof(*new_slave), GFP_KERNEL);
    if (!new_slave)
        return -ENOMEM;

    new_slave->dev = slave_dev;
    new_slave->active = true;

    spin_lock(&bond->slave_lock);
    if (bond->slave_cnt >= KCORP_BOND_MAX_SLAVES) {
        spin_unlock(&bond->slave_lock);
        kfree(new_slave);
        return -EBUSY;
    }
    bond->slaves[bond->slave_cnt++] = new_slave;
    spin_unlock(&bond->slave_lock);

    err = slave_dev->ndo_open(slave_dev);
    if (err) {
        kfree(new_slave);
        return err;
    }

    return 0;
}

/* --- Configfs virtual GPIO item release --- */

struct kcorp_gpio_bank {
    struct list_head node;
    u32 ngpio;
    char *label;
};

struct kcorp_gpio_sim_device {
    struct mutex lock;
    struct list_head bank_list;
    int id;
    bool live;
};

void kcorp_gpio_sim_unregister_platform(int id);

/**
 * kcorp_gpio_sim_item_release - Configfs item release callback for virtual GPIO
 * @vdev: Virtual GPIO device backing the configfs directory
 */
void kcorp_gpio_sim_item_release(struct kcorp_gpio_sim_device *vdev)
{
    struct list_head *pos, *next;

    if (!vdev)
        return;

    mutex_lock(&vdev->lock);
    if (vdev->live) {
        kcorp_gpio_sim_unregister_platform(vdev->id);
        vdev->live = false;
    }

    pos = vdev->bank_list.next;
    while (pos != &vdev->bank_list) {
        struct kcorp_gpio_bank *bank =
            container_of(pos, struct kcorp_gpio_bank, node);
        next = pos->next;
        kcorp_list_del(&bank->node);
        kfree(bank->label);
        kfree(bank);
        pos = next;
    }

    kfree(vdev);
    mutex_unlock(&vdev->lock);
}

/* --- Auxiliary bus non-volatile memory child registration --- */

struct kcorp_aux_device {
    const char *name;
    u32 id;
    int refcount;
    void (*release)(struct kcorp_aux_device *aux);
    bool initialized;
    bool added;
};

struct kcorp_xe_nvm_dev {
    struct kcorp_aux_device aux;
    void *bar_io;
    size_t bar_size;
};

int kcorp_bus_publish_aux(struct kcorp_aux_device *aux);

static inline int kcorp_aux_device_init(struct kcorp_aux_device *aux)
{
    if (!aux || !aux->release || !aux->name)
        return -EINVAL;
    aux->refcount = 1;
    aux->initialized = true;
    return 0;
}

static inline void kcorp_aux_device_uninit(struct kcorp_aux_device *aux)
{
    if (!aux || !aux->initialized)
        return;
    aux->refcount--;
    if (aux->refcount == 0 && aux->release)
        aux->release(aux);
}

static inline int kcorp_aux_device_add(struct kcorp_aux_device *aux)
{
    int ret = kcorp_bus_publish_aux(aux);

    if (ret == 0)
        aux->added = true;
    return ret;
}

static void kcorp_xe_nvm_aux_release(struct kcorp_aux_device *aux)
{
    struct kcorp_xe_nvm_dev *nvm =
        container_of(aux, struct kcorp_xe_nvm_dev, aux);

    kfree(nvm);
}

/**
 * kcorp_xe_nvm_aux_init - Allocate and publish an auxiliary NVM child device
 * @bar_io: Mapped MMIO aperture
 * @bar_size: Aperture size in bytes
 * @dev_id: Instance ID
 */
int kcorp_xe_nvm_aux_init(void *bar_io, size_t bar_size, u32 dev_id)
{
    struct kcorp_xe_nvm_dev *nvm;
    int err;

    nvm = kzalloc(sizeof(*nvm), GFP_KERNEL);
    if (!nvm)
        return -ENOMEM;

    nvm->bar_io = bar_io;
    nvm->bar_size = bar_size;
    nvm->aux.name = "xe_nvm";
    nvm->aux.id = dev_id;
    nvm->aux.release = kcorp_xe_nvm_aux_release;

    err = kcorp_aux_device_init(&nvm->aux);
    if (err) {
        kfree(nvm);
        return err;
    }

    err = kcorp_aux_device_add(&nvm->aux);
    if (err) {
        kcorp_aux_device_uninit(&nvm->aux);
        kfree(nvm);
        return err;
    }

    return 0;
}

/* --- Devres-managed clock lifecycle in Ethernet MAC driver --- */

struct kcorp_clk {
    const char *con_id;
    int enable_count;
    int prepare_count;
};

struct kcorp_devres_node {
    void (*action)(void *data);
    void *data;
    struct kcorp_devres_node *next;
};

struct kcorp_platform_dev {
    struct kcorp_devres_node *devres_head;
    void *drvdata;
};

struct kcorp_eth_mac_priv {
    struct kcorp_clk *tx_clk;
    void *regs;
    bool link_up;
};

static inline void kcorp_clk_disable_unprepare(struct kcorp_clk *clk)
{
    if (!clk)
        return;
    if (WARN_ON(clk->enable_count <= 0))
        return;
    clk->enable_count--;
    clk->prepare_count--;
}

static void kcorp_devm_clk_action_cb(void *data)
{
    struct kcorp_clk *clk = data;

    kcorp_clk_disable_unprepare(clk);
    kfree(clk);
}

static struct kcorp_clk *kcorp_devm_clk_get_enabled(struct kcorp_platform_dev *pdev,
                                                    const char *id)
{
    struct kcorp_clk *clk;
    struct kcorp_devres_node *dr;

    clk = kzalloc(sizeof(*clk), GFP_KERNEL);
    if (!clk)
        return NULL;
    dr = kzalloc(sizeof(*dr), GFP_KERNEL);
    if (!dr) {
        kfree(clk);
        return NULL;
    }
    clk->con_id = id;
    clk->prepare_count = 1;
    clk->enable_count = 1;

    dr->action = kcorp_devm_clk_action_cb;
    dr->data = clk;
    dr->next = pdev->devres_head;
    pdev->devres_head = dr;
    return clk;
}

int kcorp_eth_mac_probe(struct kcorp_platform_dev *pdev, void *regs)
{
    struct kcorp_eth_mac_priv *priv;

    priv = kzalloc(sizeof(*priv), GFP_KERNEL);
    if (!priv)
        return -ENOMEM;

    priv->tx_clk = kcorp_devm_clk_get_enabled(pdev, "tx_core");
    if (!priv->tx_clk) {
        kfree(priv);
        return -ENODEV;
    }
    priv->regs = regs;
    pdev->drvdata = priv;
    return 0;
}

/**
 * kcorp_eth_mac_remove - Detach Ethernet MAC before devres teardown runs
 * @pdev: Platform device undergoing driver unbind
 */
void kcorp_eth_mac_remove(struct kcorp_platform_dev *pdev)
{
    struct kcorp_eth_mac_priv *priv = pdev->drvdata;
    struct kcorp_devres_node *dr, *next;

    if (!priv)
        return;

    priv->link_up = false;
    kcorp_clk_disable_unprepare(priv->tx_clk);

    /* Core driver-detach runs registered devm cleanup callbacks right after remove(). */
    dr = pdev->devres_head;
    while (dr) {
        next = dr->next;
        dr->action(dr->data);
        kfree(dr);
        dr = next;
    }
    pdev->devres_head = NULL;
    kfree(priv);
    pdev->drvdata = NULL;
}

/* --- 9p superblock session teardown and RCU symlink lookup --- */

#define KCORP_V9FS_CACHE_SYMLINK 0x04u

struct kcorp_v9fs_session {
    struct rcu_head rcu;
    u32 cache_mode;
    char *aname;
    u32 maxdata;
};

struct kcorp_v9fs_dentry {
    struct kcorp_v9fs_session *v9ses;
    const char *cached_target;
};

struct kcorp_v9fs_superblock {
    struct kcorp_v9fs_session *s_fs_info;
    struct kcorp_v9fs_dentry *root_dentry;
};

void kcorp_v9fs_close_trans(struct kcorp_v9fs_session *v9ses);

const char *kcorp_v9fs_vfs_get_link_rcu(struct kcorp_v9fs_dentry *dentry)
{
    struct kcorp_v9fs_session *v9ses;
    const char *res = NULL;

    rcu_read_lock();
    v9ses = dentry->v9ses;
    if (v9ses && (v9ses->cache_mode & KCORP_V9FS_CACHE_SYMLINK))
        res = dentry->cached_target;
    rcu_read_unlock();
    return res;
}

/**
 * kcorp_v9fs_kill_super - Tear down 9p superblock and session state
 * @sb: Superblock being unmounted
 */
void kcorp_v9fs_kill_super(struct kcorp_v9fs_superblock *sb)
{
    struct kcorp_v9fs_session *v9ses;

    if (!sb || !sb->s_fs_info)
        return;

    v9ses = sb->s_fs_info;
    if (sb->root_dentry)
        sb->root_dentry->v9ses = NULL;
    sb->s_fs_info = NULL;

    kcorp_v9fs_close_trans(v9ses);
    kfree(v9ses->aname);
    kfree(v9ses);
}

/* --- Cgroup region statistics RCU list traversal --- */

struct kcorp_dmem_stat_node {
    struct list_head rcu_link;
    struct rcu_head rcu;
    u64 bytes_peak;
    u64 failcnt;
    u32 region_id;
};

struct kcorp_dmem_cgroup_state {
    spinlock_t list_lock;
    struct list_head stat_list;
};

static inline struct list_head *kcorp_list_next_rcu_checked(struct list_head *entry)
{
    WARN_ON(!rcu_read_lock_held());
    return entry->next;
}

void kcorp_dmem_remove_stat_node(struct kcorp_dmem_cgroup_state *cs,
                                 struct kcorp_dmem_stat_node *node)
{
    spin_lock(&cs->list_lock);
    node->rcu_link.prev->next = node->rcu_link.next;
    node->rcu_link.next->prev = node->rcu_link.prev;
    spin_unlock(&cs->list_lock);
    call_rcu_free(&node->rcu, offsetof(struct kcorp_dmem_stat_node, rcu));
}

/**
 * kcorp_dmem_fold_region_stats - Sum failure counts across RCU-protected nodes
 * @cs: Cgroup state owning the RCU-protected stat_list
 * @target_region: Region identifier filter
 */
u64 kcorp_dmem_fold_region_stats(struct kcorp_dmem_cgroup_state *cs, u32 target_region)
{
    struct list_head *pos;
    u64 total_fails = 0;

    pos = kcorp_list_next_rcu_checked(&cs->stat_list);
    while (pos != &cs->stat_list) {
        struct kcorp_dmem_stat_node *node =
            container_of(pos, struct kcorp_dmem_stat_node, rcu_link);
        if (node->region_id == target_region)
            total_fails += node->failcnt;
        pos = kcorp_list_next_rcu_checked(pos);
    }

    return total_fails;
}

/* --- MPTCP path-manager announced address table --- */

struct kcorp_mptcp_addr_entry {
    struct list_head list;
    struct rcu_head rcu;
    u32 ipv4_addr;
    u16 port;
    u8 addr_id;
};

struct kcorp_mptcp_pm {
    spinlock_t lock;
    struct list_head anno_list;
    unsigned int addrs_nr;
};

static inline void kcorp_list_splice_init(struct list_head *list,
                                          struct list_head *head)
{
    if (list->next != list) {
        struct list_head *first = list->next;
        struct list_head *last = list->prev;
        struct list_head *at = head->next;

        first->prev = head;
        head->next = first;
        last->next = at;
        at->prev = last;

        list->next = list;
        list->prev = list;
    }
}

u32 kcorp_mptcp_pm_lookup_addr_rcu(struct kcorp_mptcp_pm *pm, u8 id)
{
    struct list_head *pos;
    u32 found = 0;

    rcu_read_lock();
    for (pos = pm->anno_list.next; pos != &pm->anno_list; pos = pos->next) {
        struct kcorp_mptcp_addr_entry *entry =
            container_of(pos, struct kcorp_mptcp_addr_entry, list);
        if (entry->addr_id == id) {
            found = entry->ipv4_addr;
            break;
        }
    }
    rcu_read_unlock();
    return found;
}

/**
 * kcorp_mptcp_pm_flush_addrs - Flush all announced MPTCP endpoints via netlink
 * @pm: MPTCP path manager state
 */
void kcorp_mptcp_pm_flush_addrs(struct kcorp_mptcp_pm *pm)
{
    struct list_head free_list;
    struct list_head *pos, *next;

    kcorp_list_init(&free_list);

    spin_lock(&pm->lock);
    kcorp_list_splice_init(&pm->anno_list, &free_list);
    pm->addrs_nr = 0;
    spin_unlock(&pm->lock);

    pos = free_list.next;
    while (pos != &free_list) {
        struct kcorp_mptcp_addr_entry *entry =
            container_of(pos, struct kcorp_mptcp_addr_entry, list);
        next = pos->next;
        kfree(entry);
        pos = next;
    }
}

/* --- KVM MMIO I/O bus lookup and SRCU lifetime --- */

#define KCORP_KVM_MAX_IO_DEVS 32

struct kcorp_kvm_io_dev {
    u64 base_gpa;
    u64 len;
    int (*read)(struct kcorp_kvm_io_dev *dev, u64 gpa, u64 *val);
};

struct kcorp_kvm_io_bus {
    unsigned int dev_count;
    struct kcorp_kvm_io_dev *devs[KCORP_KVM_MAX_IO_DEVS];
};

struct kcorp_kvm_vm_bus {
    struct mutex slots_lock;
    struct srcu_struct bus_srcu;
    struct kcorp_kvm_io_bus *mmio_bus;
};

void kcorp_kvm_io_bus_unregister(struct kcorp_kvm_vm_bus *vm,
                                 struct kcorp_kvm_io_dev *target)
{
    struct kcorp_kvm_io_bus *old_bus, *new_bus;
    unsigned int i, dst = 0;

    mutex_lock(&vm->slots_lock);
    old_bus = vm->mmio_bus;
    new_bus = kzalloc(sizeof(*new_bus), GFP_KERNEL);
    if (!new_bus) {
        mutex_unlock(&vm->slots_lock);
        return;
    }
    for (i = 0; i < old_bus->dev_count; i++) {
        if (old_bus->devs[i] != target)
            new_bus->devs[dst++] = old_bus->devs[i];
    }
    new_bus->dev_count = dst;
    vm->mmio_bus = new_bus;
    mutex_unlock(&vm->slots_lock);

    synchronize_srcu(&vm->bus_srcu);
    kfree(old_bus);
    kfree(target);
}

/**
 * kcorp_kvm_io_bus_get_dev - Locate MMIO device covering guest physical address
 * @vm: Guest VM bus container
 * @gpa: Faulting guest physical address
 */
struct kcorp_kvm_io_dev *kcorp_kvm_io_bus_get_dev(struct kcorp_kvm_vm_bus *vm, u64 gpa)
{
    struct kcorp_kvm_io_bus *bus;
    struct kcorp_kvm_io_dev *matched = NULL;
    unsigned int i;
    int srcu_idx;

    srcu_idx = srcu_read_lock(&vm->bus_srcu);
    bus = vm->mmio_bus;
    if (bus) {
        for (i = 0; i < bus->dev_count; i++) {
            struct kcorp_kvm_io_dev *dev = bus->devs[i];
            if (gpa >= dev->base_gpa && gpa < dev->base_gpa + dev->len) {
                matched = dev;
                break;
            }
        }
    }
    srcu_read_unlock(&vm->bus_srcu, srcu_idx);
    return matched;
}

int kcorp_kvm_dispatch_mmio_read(struct kcorp_kvm_vm_bus *vm, u64 gpa, u64 *val)
{
    struct kcorp_kvm_io_dev *dev = kcorp_kvm_io_bus_get_dev(vm, gpa);

    if (!dev)
        return -EOPNOTSUPP;
    return dev->read(dev, gpa, val);
}

/* --- Flex-proportions period aging and hardirq reader --- */

struct kcorp_fprop_global {
    spinlock_t lock;
    struct seqcount sequence;
    unsigned int period;
    u64 events_sum;
};

static inline void kcorp_write_seqcount_begin(struct seqcount *s)
{
    s->sequence++;
}

static inline void kcorp_write_seqcount_end(struct seqcount *s)
{
    s->sequence++;
}

static inline unsigned int kcorp_read_seqcount_begin(const struct seqcount *s)
{
    unsigned int ret;

    /* Spins until writer finishes odd sequence phase. */
    while ((ret = s->sequence) & 1u) {
    }
    return ret;
}

static inline bool kcorp_read_seqcount_retry(const struct seqcount *s, unsigned int start)
{
    return s->sequence != start;
}

/* Invoked from hardware interrupt context (e.g. block completion hardirq). */
u64 kcorp_fprop_hardirq_read_sum(const struct kcorp_fprop_global *fp, unsigned int *period_out)
{
    unsigned int seq;
    u64 sum;
    unsigned int per;

    do {
        seq = kcorp_read_seqcount_begin(&fp->sequence);
        sum = fp->events_sum;
        per = fp->period;
    } while (kcorp_read_seqcount_retry(&fp->sequence, seq));

    *period_out = per;
    return sum;
}

/**
 * kcorp_fprop_new_period - Advance global flex-proportion decay period
 * @fp: Global proportion state
 * @periods: Number of periods elapsed
 */
bool kcorp_fprop_new_period(struct kcorp_fprop_global *fp, unsigned int periods)
{
    if (periods == 0)
        return false;

    spin_lock(&fp->lock);
    kcorp_write_seqcount_begin(&fp->sequence);
    if (periods < 64)
        fp->events_sum >>= periods;
    else
        fp->events_sum = 0;
    fp->period += periods;
    kcorp_write_seqcount_end(&fp->sequence);
    spin_unlock(&fp->lock);
    return true;
}

/* --- IOMMU domain attachment and concurrent TLB shootdown --- */

struct kcorp_rk_iommu {
    struct list_head node;
    void **reg_bases;
    unsigned int num_mmu;
    u32 irq_mask;
};

struct kcorp_rk_iommu_domain {
    spinlock_t iommus_lock;
    struct list_head iommus;
    u32 dte_addr;
};

void kcorp_iommu_writel(void *base, u32 offset, u32 val);
void *kcorp_dev_ioremap_mmu(unsigned int idx);

void kcorp_rk_iommu_zap_iova(struct kcorp_rk_iommu_domain *domain, dma_addr_t iova)
{
    struct list_head *pos;
    unsigned long flags;
    unsigned int i;

    spin_lock_irqsave(&domain->iommus_lock, &flags);
    for (pos = domain->iommus.next; pos != &domain->iommus; pos = pos->next) {
        struct kcorp_rk_iommu *iommu =
            container_of(pos, struct kcorp_rk_iommu, node);
        for (i = 0; i < iommu->num_mmu; i++)
            kcorp_iommu_writel(iommu->reg_bases[i], 0x10, (u32)iova);
    }
    spin_unlock_irqrestore(&domain->iommus_lock, flags);
}

/**
 * kcorp_rk_iommu_attach_device - Attach an IOMMU instance to a translation domain
 * @domain: Target IOMMU domain
 * @iommu: Uninitialized IOMMU hardware instance
 * @mmu_cnt: Number of hardware MMU blocks
 */
int kcorp_rk_iommu_attach_device(struct kcorp_rk_iommu_domain *domain,
                                 struct kcorp_rk_iommu *iommu,
                                 unsigned int mmu_cnt)
{
    unsigned long flags;
    unsigned int i;

    if (!domain || !iommu || mmu_cnt == 0)
        return -EINVAL;

    iommu->num_mmu = mmu_cnt;

    spin_lock_irqsave(&domain->iommus_lock, &flags);
    kcorp_list_add_tail(&iommu->node, &domain->iommus);
    spin_unlock_irqrestore(&domain->iommus_lock, flags);

    iommu->reg_bases = kcalloc(mmu_cnt, sizeof(void *), GFP_KERNEL);
    if (!iommu->reg_bases)
        return -ENOMEM;

    for (i = 0; i < mmu_cnt; i++) {
        iommu->reg_bases[i] = kcorp_dev_ioremap_mmu(i);
        kcorp_iommu_writel(iommu->reg_bases[i], 0x00, domain->dte_addr);
    }
    return 0;
}

/* --- FireWire asynchronous transmit/receive transaction list --- */

struct kcorp_fw_transaction {
    struct list_head link;
    int tlabel;
    int rcode;
    bool completed;
    void (*callback)(struct kcorp_fw_transaction *t, int rcode);
};

struct kcorp_fw_card {
    spinlock_t lock;
    struct list_head transaction_list;
    u64 tlabel_mask;
};

void kcorp_fw_ar_handle_response(struct kcorp_fw_card *card, int tlabel, int rcode)
{
    struct kcorp_fw_transaction *t = NULL;
    struct list_head *pos;
    unsigned long flags;

    spin_lock_irqsave(&card->lock, &flags);
    for (pos = card->transaction_list.next; pos != &card->transaction_list; pos = pos->next) {
        struct kcorp_fw_transaction *iter =
            container_of(pos, struct kcorp_fw_transaction, link);
        if (iter->tlabel == tlabel) {
            t = iter;
            kcorp_list_del(&t->link);
            card->tlabel_mask &= ~(1ULL << tlabel);
            t->completed = true;
            break;
        }
    }
    spin_unlock_irqrestore(&card->lock, flags);

    if (t && t->callback)
        t->callback(t, rcode);
}

/**
 * kcorp_fw_at_complete_packet - Handle local AT DMA completion failure or timeout
 * @card: FireWire OHCI card instance
 * @t: In-flight transaction whose AT packet finished with transport error
 * @ack_err: Hardware ACK status
 */
void kcorp_fw_at_complete_packet(struct kcorp_fw_card *card,
                                 struct kcorp_fw_transaction *t,
                                 int ack_err)
{
    unsigned long flags;

    if (ack_err == 0)
        return;

    /* Remove failed split transaction immediately and release its tlabel. */
    kcorp_list_del(&t->link);

    spin_lock_irqsave(&card->lock, &flags);
    card->tlabel_mask &= ~(1ULL << t->tlabel);
    t->completed = true;
    spin_unlock_irqrestore(&card->lock, flags);

    if (t->callback)
        t->callback(t, ack_err);
}

/* --- KVM nested stage-2 page-table unmap and shadow flush --- */

struct kcorp_kvm_pgtable {
    u32 ia_bits;
    u32 start_level;
    void *root_table;
};

struct kcorp_nested_s2_mmu {
    struct kcorp_kvm_pgtable *pgt;
    bool active;
};

struct kcorp_kvm_mmu_vm {
    spinlock_t mmu_lock;
    struct kcorp_kvm_pgtable *s2_pgt;
    struct kcorp_nested_s2_mmu nested_mmus[8];
    unsigned int nested_cnt;
    bool need_resched;
};

void kcorp_pgtable_unmap_range(struct kcorp_kvm_pgtable *pgt, u64 start, u64 size);
void kcorp_pgtable_destroy_tlb(struct kcorp_kvm_pgtable *pgt, u32 ia_bits);

void kcorp_kvm_teardown_shadow_mmu(struct kcorp_kvm_mmu_vm *vm)
{
    spin_lock(&vm->mmu_lock);
    if (vm->s2_pgt) {
        kfree(vm->s2_pgt);
        vm->s2_pgt = NULL;
    }
    spin_unlock(&vm->mmu_lock);
}

/**
 * kcorp_kvm_nested_s2_unmap - Unmap IPA range across all nested stage-2 tables
 * @vm: Guest VM MMU state
 * @ipa: Guest IPA start
 * @size: Unmap range length
 */
u32 kcorp_kvm_nested_s2_unmap(struct kcorp_kvm_mmu_vm *vm, u64 ipa, u64 size)
{
    unsigned int i;
    u32 canonical_bits;

    spin_lock(&vm->mmu_lock);
    if (!vm->s2_pgt) {
        spin_unlock(&vm->mmu_lock);
        return 0;
    }

    for (i = 0; i < vm->nested_cnt; i++) {
        if (vm->nested_mmus[i].active && vm->nested_mmus[i].pgt)
            kcorp_pgtable_unmap_range(vm->nested_mmus[i].pgt, ipa, size);

        if (vm->need_resched) {
            spin_unlock(&vm->mmu_lock);
            cond_resched();
            spin_lock(&vm->mmu_lock);
        }
    }

    canonical_bits = vm->s2_pgt->ia_bits;
    spin_unlock(&vm->mmu_lock);
    return canonical_bits;
}

/* --- BPF hash map batch lookup and delete syscall handler --- */

struct kcorp_bpf_htab_elem {
    struct kcorp_bpf_htab_elem *next;
    u64 key;
    u64 value;
};

struct kcorp_bpf_pcpu_freelist {
    struct kcorp_bpf_htab_elem *head;
    u64 recycled_cnt;
};

struct kcorp_bpf_htab {
    spinlock_t bucket_lock;
    struct kcorp_bpf_htab_elem *buckets[64];
    struct kcorp_bpf_pcpu_freelist pcpu_free[8];
    unsigned int current_cpu;
};

static inline struct kcorp_bpf_pcpu_freelist *
kcorp_this_cpu_freelist(struct kcorp_bpf_htab *htab)
{
    /* Per-CPU accessor requires preemption disabled to prevent CPU migration. */
    WARN_ON(preemptible());
    return &htab->pcpu_free[htab->current_cpu & 7u];
}

/**
 * kcorp_rhtab_delete_batch - Export and recycle hash entries for userspace batch
 * @htab: Hash table instance
 * @user_keys: Userspace destination buffer for deleted keys
 * @max_entries: Batch capacity
 */
int kcorp_rhtab_delete_batch(struct kcorp_bpf_htab *htab,
                             void *user_keys,
                             unsigned int max_entries)
{
    unsigned int b, exported = 0;

    for (b = 0; b < 64 && exported < max_entries; b++) {
        struct kcorp_bpf_htab_elem *elem;
        struct kcorp_bpf_pcpu_freelist *fl;

        spin_lock(&htab->bucket_lock);
        elem = htab->buckets[b];
        if (elem)
            htab->buckets[b] = elem->next;
        spin_unlock(&htab->bucket_lock);

        if (!elem)
            continue;

        if (copy_to_user((char *)user_keys + exported * sizeof(u64),
                         &elem->key, sizeof(u64)) != 0) {
            kfree(elem);
            return -EFAULT;
        }

        fl = kcorp_this_cpu_freelist(htab);
        elem->next = fl->head;
        fl->head = elem;
        fl->recycled_cnt++;
        exported++;
    }

    return (int)exported;
}

/* --- Memory management deferred struct page initialization --- */

struct kcorp_page_frame {
    u64 flags;
    int nid;
    int refcount;
};

struct kcorp_mem_zone {
    spinlock_t init_lock;
    struct kcorp_page_frame *mem_map;
    unsigned long first_deferred_pfn;
    unsigned long end_pfn;
    int node_id;
};

/**
 * kcorp_deferred_init_memmap_chunk - Initialize deferred struct page range
 * @zone: Memory zone being initialized at boot
 * @max_nr_pages: Maximum pages to initialize in this batch
 */
unsigned long kcorp_deferred_init_memmap_chunk(struct kcorp_mem_zone *zone,
                                               unsigned long max_nr_pages)
{
    unsigned long pfn, initialized = 0;

    spin_lock(&zone->init_lock);
    pfn = zone->first_deferred_pfn;
    while (pfn < zone->end_pfn && initialized < max_nr_pages) {
        struct kcorp_page_frame *page = &zone->mem_map[pfn];

        page->flags = 0;
        page->nid = zone->node_id;
        page->refcount = 1;
        pfn++;
        initialized++;

        if ((initialized & 0x1ffUL) == 0)
            cond_resched();
    }
    zone->first_deferred_pfn = pfn;
    spin_unlock(&zone->init_lock);

    return initialized;
}

/* --- WWAN modem DPMAIF scatter-gather RX packet assembly --- */

#define KCORP_MAX_SKB_FRAGS 17

struct kcorp_skb_frag {
    u64 page_phys;
    u32 offset;
    u32 size;
};

struct kcorp_skb_shared_info {
    unsigned int nr_frags;
    u32 data_len;
    struct kcorp_skb_frag frags[KCORP_MAX_SKB_FRAGS];
};

struct kcorp_dpmaif_bat_pit {
    u64 payload_phys;
    u32 payload_offset;
    u32 payload_len;
    bool cont;
};

/**
 * kcorp_t7xx_dpmaif_append_frag - Attach BAT buffer descriptors as skb fragments
 * @shinfo: Target skb shared info
 * @pits: Array of hardware PIT descriptors for a single packet
 * @pit_cnt: Number of PIT descriptors in packet
 */
int kcorp_t7xx_dpmaif_append_frag(struct kcorp_skb_shared_info *shinfo,
                                  const struct kcorp_dpmaif_bat_pit *pits,
                                  unsigned int pit_cnt)
{
    unsigned int i;

    if (!shinfo || !pits)
        return -EINVAL;

    for (i = 0; i < pit_cnt; i++) {
        struct kcorp_skb_frag *frag;

        if (pits[i].payload_len == 0)
            continue;

        frag = &shinfo->frags[shinfo->nr_frags];
        frag->page_phys = pits[i].payload_phys;
        frag->offset = pits[i].payload_offset;
        frag->size = pits[i].payload_len;
        shinfo->nr_frags++;
        shinfo->data_len += pits[i].payload_len;

        if (!pits[i].cont)
            break;
    }

    return 0;
}

/* --- IEEE 802.11be Tid-To-Link Mapping (TTLM) element parser --- */

#define KCORP_TTLM_CTRL_DIRECTION_MASK  0x03u
#define KCORP_TTLM_CTRL_DEF_LINK_MAP    0x04u
#define KCORP_TTLM_CTRL_SWITCH_TIME     0x08u

struct kcorp_parsed_ttlm {
    u8 direction;
    bool default_mapping;
    u16 switch_time;
    u8 link_map_presence;
    u16 tid_links[8];
};

/**
 * kcorp_ieee80211_parse_ttlm - Parse an 802.11be TTLM extension element payload
 * @data: Raw IE bytes starting at the Control field
 * @elem_len: Length of @data in bytes
 * @out: Parsed output structure
 */
int kcorp_ieee80211_parse_ttlm(const u8 *data, size_t elem_len, struct kcorp_parsed_ttlm *out)
{
    size_t pos = 0;
    u8 ctrl;
    unsigned int tid;

    if (!data || !out || elem_len < 1)
        return -EINVAL;

    memset(out, 0, sizeof(*out));
    ctrl = data[pos++];
    out->direction = ctrl & KCORP_TTLM_CTRL_DIRECTION_MASK;
    out->default_mapping = !!(ctrl & KCORP_TTLM_CTRL_DEF_LINK_MAP);

    if (ctrl & KCORP_TTLM_CTRL_SWITCH_TIME) {
        if (pos + 2 > elem_len)
            return -EINVAL;
        out->switch_time = (u16)data[pos] | ((u16)data[pos + 1] << 8);
        pos += 2;
    }

    /*
     * Per 802.11be spec, when DEF_LINK_MAP is set, the Link Mapping Presence
     * Indicator octet is omitted from the frame and elem_len may equal pos.
     */
    out->link_map_presence = data[pos++];
    if (out->default_mapping) {
        for (tid = 0; tid < 8; tid++)
            out->tid_links[tid] = 0xffffu;
        return 0;
    }

    for (tid = 0; tid < 8; tid++) {
        if (!(out->link_map_presence & (1u << tid)))
            continue;
        if (pos + 2 > elem_len)
            return -EINVAL;
        out->tid_links[tid] = (u16)data[pos] | ((u16)data[pos + 1] << 8);
        pos += 2;
    }

    return 0;
}

/* --- SoC CPUFreq DT match table lookup --- */

struct kcorp_of_device_id {
    char compatible[64];
    const void *data;
};

struct kcorp_soc_opp_cfg {
    u32 max_freq_khz;
    u32 num_domains;
};

static const struct kcorp_soc_opp_cfg kcorp_ipq8064_cfg = { 1400000, 2 };
static const struct kcorp_soc_opp_cfg kcorp_ipq8065_cfg = { 1700000, 2 };

static const struct kcorp_of_device_id kcorp_ipq_cpufreq_match_list[] = {
    { .compatible = "qcom,ipq8062", .data = &kcorp_ipq8064_cfg },
    { .compatible = "qcom,ipq8064", .data = &kcorp_ipq8064_cfg },
    { .compatible = "qcom,ipq8065", .data = &kcorp_ipq8065_cfg },
    { .compatible = "qcom,ipq8069", .data = &kcorp_ipq8065_cfg },
};

static const struct kcorp_of_device_id *
kcorp_of_match_device_table(const struct kcorp_of_device_id *matches,
                            const char *node_compat)
{
    if (!matches || !node_compat)
        return NULL;

    while (matches->compatible[0] != '\0') {
        if (strcmp(matches->compatible, node_compat) == 0)
            return matches;
        matches++;
    }
    return NULL;
}

/**
 * kcorp_ipq_cpufreq_probe_soc - Look up OPP configuration for the running SoC
 * @compat: Root device-tree compatible string
 */
const struct kcorp_soc_opp_cfg *kcorp_ipq_cpufreq_probe_soc(const char *compat)
{
    const struct kcorp_of_device_id *match;

    match = kcorp_of_match_device_table(kcorp_ipq_cpufreq_match_list, compat);
    if (!match)
        return NULL;
    return match->data;
}

/* --- DRM MIPI-DSI host controller FIFO read --- */

struct kcorp_dsi_host {
    const u8 *rx_iomem;
    size_t rx_map_size;
    u32 max_pktsize;
};

/**
 * kcorp_dsi_host_read_rx_fifo - Copy DSI peripheral response from mapped FIFO
 * @host: DSI host controller with mapped rx_iomem region of rx_map_size bytes
 * @dst: Caller destination buffer (at least ALIGN(req_bytes, 16) bytes)
 * @req_bytes: Requested response byte count
 */
int kcorp_dsi_host_read_rx_fifo(struct kcorp_dsi_host *host, u8 *dst, size_t req_bytes)
{
    size_t clamped, burst_len, off;

    if (!host || !host->rx_iomem || !dst || host->rx_map_size == 0)
        return -EINVAL;

    clamped = req_bytes;
    if (clamped > host->rx_map_size)
        clamped = host->rx_map_size;

    /* Hardware FIFO transfers 16-byte bursts; align transfer length upward. */
    burst_len = ALIGN(clamped, 16);
    for (off = 0; off < burst_len; off += 4) {
        dst[off + 0] = host->rx_iomem[off + 0];
        dst[off + 1] = host->rx_iomem[off + 1];
        dst[off + 2] = host->rx_iomem[off + 2];
        dst[off + 3] = host->rx_iomem[off + 3];
    }

    return (int)clamped;
}

/* --- SMB client synchronous request/response transport --- */

#define KCORP_CIFS_MAX_MSGSIZE 65536

struct kcorp_cifs_ses {
    u64 ses_id;
    unsigned int server_credits;
    bool need_reconnect;
};

struct kcorp_smb_hdr {
    u32 protocol_id;
    u16 command;
    u32 status;
};

int kcorp_cifs_send_recv_wire(struct kcorp_cifs_ses *ses,
                              const struct kcorp_smb_hdr *in_buf,
                              u8 *out_buf,
                              size_t *out_len);
int kcorp_map_smb_status_to_posix(u32 smb_status);

/**
 * kcorp_cifs_send_receive_cmd - Exchange an SMB control frame and copy reply
 * @ses: Active SMB session
 * @req: Outgoing request header
 * @reply_out: Caller buffer for validated response header
 */
int kcorp_cifs_send_receive_cmd(struct kcorp_cifs_ses *ses,
                                const struct kcorp_smb_hdr *req,
                                struct kcorp_smb_hdr *reply_out)
{
    u8 *resp_buf;
    size_t rcv_len = 0;
    int rc;

    if (!ses || !req || !reply_out)
        return -EINVAL;
    if (ses->need_reconnect)
        return -EAGAIN;

    resp_buf = kzalloc(KCORP_CIFS_MAX_MSGSIZE, GFP_KERNEL);
    if (!resp_buf)
        return -ENOMEM;

    rc = kcorp_cifs_send_recv_wire(ses, req, resp_buf, &rcv_len);
    if (rc < 0)
        return rc;

    if (rcv_len < sizeof(*reply_out)) {
        kfree(resp_buf);
        return -EIO;
    }

    memcpy(reply_out, resp_buf, sizeof(*reply_out));
    rc = kcorp_map_smb_status_to_posix(reply_out->status);
    kfree(resp_buf);
    return rc;
}

/* --- SMB2 CREATE / open replay retry handler --- */

struct kcorp_smb2_err_rsp {
    u32 byte_count;
    u8 error_data[256];
};

int kcorp_smb2_raw_open(u64 tcon_id,
                        const char *utf8_path,
                        u64 *fid_out,
                        struct kcorp_smb2_err_rsp **err_rsp_out);

/**
 * kcorp_smb2_open_file_retry - Issue SMB2 CREATE with credit/replay retries
 * @tcon_id: Tree connection ID
 * @path: Share-relative path
 * @fid_out: Returned persistent file ID
 */
int kcorp_smb2_open_file_retry(u64 tcon_id, const char *path, u64 *fid_out)
{
    struct kcorp_smb2_err_rsp *err_buf = NULL;
    unsigned int attempts = 0;
    int rc;

retry_open:
    rc = kcorp_smb2_raw_open(tcon_id, path, fid_out, &err_buf);
    if (rc == -EAGAIN && attempts < 3) {
        /* Note: kcorp_smb2_raw_open allocates *err_rsp_out whenever server sends an error PDU. */
        attempts++;
        err_buf = NULL;
        goto retry_open;
    }

    kfree(err_buf);
    return rc;
}

/* --- KVM stage-2 page fault handler --- */

#define KCORP_ESR_FSC_MASK        0x3fu
#define KCORP_ESR_FSC_EXCL_ATOMIC 0x35u

struct kcorp_host_page {
    phys_addr_t phys;
    int refcount;
    bool dirty;
};

struct kcorp_kvm_vcpu {
    u64 fault_ipa;
    u32 fault_esr;
    u32 pending_abt;
};

int kcorp_kvm_stage2_map_page(struct kcorp_kvm_vcpu *vcpu,
                              u64 ipa,
                              struct kcorp_host_page *page);
void kcorp_kvm_inject_dabt(struct kcorp_kvm_vcpu *vcpu);

static inline int kcorp_kvm_faultin_pfn(struct kcorp_kvm_vcpu *vcpu,
                                        u64 ipa,
                                        struct kcorp_host_page **page_out)
{
    struct kcorp_host_page *page = kzalloc(sizeof(*page), GFP_KERNEL);

    (void)vcpu;
    if (!page)
        return -ENOMEM;
    page->phys = ipa & ~0xfffULL;
    page->refcount = 1;
    *page_out = page;
    return 0;
}

static inline void kcorp_kvm_release_page_clean(struct kcorp_host_page *page)
{
    if (!page)
        return;
    page->refcount--;
    if (page->refcount <= 0)
        kfree(page);
}

/**
 * kcorp_kvm_user_mem_abort - Resolve guest stage-2 translation or permission fault
 * @vcpu: Faulting guest vCPU
 */
int kcorp_kvm_user_mem_abort(struct kcorp_kvm_vcpu *vcpu)
{
    struct kcorp_host_page *pinned_page = NULL;
    u32 fsc = vcpu->fault_esr & KCORP_ESR_FSC_MASK;
    int ret;

    ret = kcorp_kvm_faultin_pfn(vcpu, vcpu->fault_ipa, &pinned_page);
    if (ret < 0)
        return ret;

    if (fsc == KCORP_ESR_FSC_EXCL_ATOMIC) {
        /* Unsupported exclusive atomic to non-cacheable slot: forward abort to guest. */
        kcorp_kvm_inject_dabt(vcpu);
        return 1;
    }

    ret = kcorp_kvm_stage2_map_page(vcpu, vcpu->fault_ipa, pinned_page);
    kcorp_kvm_release_page_clean(pinned_page);
    return ret == 0 ? 1 : ret;
}

/* --- Cgroup dmem.max control file write parser --- */

int kcorp_dmem_apply_region_max(const char *region_name, u64 limit_bytes);

static char *kcorp_strsep_ws(char **stringp)
{
    char *start = *stringp;
    char *p;

    if (!start)
        return NULL;
    for (p = start; *p != '\0'; p++) {
        if (*p == ' ' || *p == '\t' || *p == '\n') {
            *p = '\0';
            *stringp = p + 1;
            return start;
        }
    }
    *stringp = NULL;
    return start;
}

static int kcorp_page_counter_memparse(const char *buf, u64 *limit_out)
{
    u64 val = 0;
    const char *p = buf;

    /* Dereferences buf directly; caller must pass a non-NULL token string. */
    if (strcmp(buf, "max") == 0) {
        *limit_out = UINT64_MAX;
        return 0;
    }
    if (*p == '\0')
        return -EINVAL;
    while (*p >= '0' && *p <= '9') {
        val = val * 10 + (u64)(*p - '0');
        p++;
    }
    if (*p != '\0')
        return -EINVAL;
    *limit_out = val;
    return 0;
}

/**
 * kcorp_dmem_max_write - Parse "<region_name> <max|bytes>" from cgroup write
 * @raw_input: Mutable NUL-terminated buffer written by userspace
 */
int kcorp_dmem_max_write(char *raw_input)
{
    char *cursor = raw_input;
    char *region_tok;
    u64 limit = 0;
    int err;

    if (!raw_input)
        return -EINVAL;

    region_tok = kcorp_strsep_ws(&cursor);
    if (!region_tok || region_tok[0] == '\0')
        return -EINVAL;

    err = kcorp_page_counter_memparse(cursor, &limit);
    if (err)
        return err;

    return kcorp_dmem_apply_region_max(region_tok, limit);
}

/* --- NVMe PCI scatter-gather list (SGL) DMA unmapping --- */

#define KCORP_SGL_TYPE_DATA_DESC     0x00u
#define KCORP_SGL_TYPE_LAST_SEG_DESC 0x03u

struct kcorp_nvme_sgl_desc {
    dma_addr_t addr;
    u32 length;
    u8 rsvd[3];
    u8 sgl_type;
};

struct kcorp_nvme_dev {
    u32 ctrl_id;
    u64 unmapped_bytes;
};

struct kcorp_nvme_iod {
    struct kcorp_nvme_sgl_desc cmd_sge;
    struct kcorp_nvme_sgl_desc *seg_table_list;
    unsigned int nr_descriptors;
};

void kcorp_dma_pool_free_seg(struct kcorp_nvme_dev *dev,
                             struct kcorp_nvme_sgl_desc *seg_table,
                             dma_addr_t table_dma);

static void kcorp_nvme_free_sgls(struct kcorp_nvme_dev *dev,
                                 struct kcorp_nvme_sgl_desc *data_sge,
                                 struct kcorp_nvme_sgl_desc *seg_list)
{
    /*
     * @data_sge is the command-inline Last-Segment SGE holding the DMA address
     * and byte length of @seg_list; @seg_list is the heap/pool segment array.
     */
    if (data_sge->sgl_type != KCORP_SGL_TYPE_LAST_SEG_DESC)
        return;

    dev->unmapped_bytes += data_sge->length;
    kcorp_dma_pool_free_seg(dev, seg_list, data_sge->addr);
}

/**
 * kcorp_nvme_unmap_iod_sgls - Tear down SGL DMA mappings after NVMe completion
 * @dev: NVMe controller
 * @iod: Completed I/O descriptor
 */
void kcorp_nvme_unmap_iod_sgls(struct kcorp_nvme_dev *dev, struct kcorp_nvme_iod *iod)
{
    if (!dev || !iod || !iod->seg_table_list)
        return;

    kcorp_nvme_free_sgls(dev, iod->seg_table_list, &iod->cmd_sge);
    iod->seg_table_list = NULL;
}

/* --- IPv4 route nexthop cache with RCU grace-period reclamation --- */

struct kcorp_route_entry {
    struct rcu_head rcu;
    u32 dst_prefix;
    u32 gateway;
    u32 ifindex;
};

struct kcorp_route_slot {
    spinlock_t update_lock;
    struct kcorp_route_entry *current_rt;
};

u32 kcorp_route_lookup_gateway_rcu(const struct kcorp_route_slot *slot, u32 prefix)
{
    const struct kcorp_route_entry *rt;
    u32 gw = 0;

    rcu_read_lock();
    rt = slot->current_rt;
    if (rt && rt->dst_prefix == prefix)
        gw = rt->gateway;
    rcu_read_unlock();
    return gw;
}

/**
 * kcorp_route_entry_replace - Atomically swap cached route and defer free via RCU
 * @slot: Route cache slot
 * @prefix: Destination prefix
 * @gateway: New nexthop gateway
 * @ifindex: Outgoing interface index
 */
int kcorp_route_entry_replace(struct kcorp_route_slot *slot,
                              u32 prefix,
                              u32 gateway,
                              u32 ifindex)
{
    struct kcorp_route_entry *new_rt, *old_rt;

    if (!slot)
        return -EINVAL;

    new_rt = kzalloc(sizeof(*new_rt), GFP_KERNEL);
    if (!new_rt)
        return -ENOMEM;

    new_rt->dst_prefix = prefix;
    new_rt->gateway = gateway;
    new_rt->ifindex = ifindex;

    spin_lock(&slot->update_lock);
    old_rt = slot->current_rt;
    slot->current_rt = new_rt;
    spin_unlock(&slot->update_lock);

    if (old_rt)
        call_rcu_free(&old_rt->rcu, offsetof(struct kcorp_route_entry, rcu));
    return 0;
}

/* --- Bluetooth HCI controller teardown with ordered rfkill & workqueue stop --- */

struct kcorp_bt_hci_dev {
    struct mutex lock;
    struct kcorp_rfkill *rfk;
    struct workqueue_struct *power_wq;
    struct work_struct power_off_work;
    bool powered;
};

static int kcorp_bt_rfkill_block_cb(void *data, bool blocked)
{
    struct kcorp_bt_hci_dev *hdev = data;

    if (!blocked)
        return 0;
    mutex_lock(&hdev->lock);
    if (hdev->powered) {
        hdev->powered = false;
        queue_work(hdev->power_wq, &hdev->power_off_work);
    }
    mutex_unlock(&hdev->lock);
    return 0;
}

int kcorp_bt_hci_register_dev(struct kcorp_bt_hci_dev *hdev, struct workqueue_struct *wq)
{
    struct kcorp_rfkill *rfk;

    if (!hdev || !wq)
        return -EINVAL;

    rfk = kzalloc(sizeof(*rfk), GFP_KERNEL);
    if (!rfk)
        return -ENOMEM;

    rfk->priv = hdev;
    rfk->set_block = kcorp_bt_rfkill_block_cb;
    rfk->registered = true;
    hdev->power_wq = wq;
    hdev->rfk = rfk;
    hdev->powered = true;
    return 0;
}

/**
 * kcorp_bt_hci_unregister_dev - Unregister HCI controller cleanly
 * @hdev: Bluetooth HCI controller
 */
void kcorp_bt_hci_unregister_dev(struct kcorp_bt_hci_dev *hdev)
{
    if (!hdev)
        return;

    /* Unregister rfkill first so no further block callbacks can queue work. */
    if (hdev->rfk) {
        kcorp_rfkill_unregister(hdev->rfk);
        kcorp_rfkill_destroy(hdev->rfk);
        hdev->rfk = NULL;
    }

    cancel_work_sync(&hdev->power_off_work);
    if (hdev->power_wq) {
        destroy_workqueue(hdev->power_wq);
        hdev->power_wq = NULL;
    }
}

/* --- Network packet rate estimator with IRQ-safe seqcount update --- */

struct kcorp_pkt_rate_est {
    spinlock_t lock;
    struct seqcount seq;
    u64 bytes_per_sec;
    u64 last_bytes;
    u32 epoch;
};

u64 kcorp_pkt_rate_sample_irq(const struct kcorp_pkt_rate_est *est, u32 *epoch_out)
{
    unsigned int start;
    u64 bps;
    u32 ep;

    do {
        start = kcorp_read_seqcount_begin(&est->seq);
        bps = est->bytes_per_sec;
        ep = est->epoch;
    } while (kcorp_read_seqcount_retry(&est->seq, start));

    *epoch_out = ep;
    return bps;
}

/**
 * kcorp_pkt_rate_advance_epoch - Update rate estimator safely against hardirqs
 * @est: Estimator instance
 * @cur_bytes: Monotonic byte counter snapshot
 */
void kcorp_pkt_rate_advance_epoch(struct kcorp_pkt_rate_est *est, u64 cur_bytes)
{
    unsigned long irq_flags;

    if (!est)
        return;

    spin_lock_irqsave(&est->lock, &irq_flags);
    kcorp_write_seqcount_begin(&est->seq);
    if (cur_bytes >= est->last_bytes)
        est->bytes_per_sec = cur_bytes - est->last_bytes;
    est->last_bytes = cur_bytes;
    est->epoch++;
    kcorp_write_seqcount_end(&est->seq);
    spin_unlock_irqrestore(&est->lock, irq_flags);
}

/* --- Compute accelerator auxiliary child probe with distinct error labels --- */

struct kcorp_accel_aux_wrap {
    struct kcorp_aux_device aux;
    void *fw_ctx;
};

void *kcorp_accel_alloc_fw_ctx(u32 id);
void kcorp_accel_free_fw_ctx(void *ctx);

static void kcorp_accel_aux_release(struct kcorp_aux_device *aux)
{
    struct kcorp_accel_aux_wrap *wrap =
        container_of(aux, struct kcorp_accel_aux_wrap, aux);

    kcorp_accel_free_fw_ctx(wrap->fw_ctx);
    kfree(wrap);
}

/**
 * kcorp_accel_aux_probe - Initialize and add accelerator auxiliary device
 * @dev_id: Instance number
 */
int kcorp_accel_aux_probe(u32 dev_id)
{
    struct kcorp_accel_aux_wrap *wrap;
    int ret;

    wrap = kzalloc(sizeof(*wrap), GFP_KERNEL);
    if (!wrap)
        return -ENOMEM;

    wrap->fw_ctx = kcorp_accel_alloc_fw_ctx(dev_id);
    if (!wrap->fw_ctx) {
        ret = -ENOMEM;
        goto err_free_wrap;
    }

    wrap->aux.name = "accel_telemetry";
    wrap->aux.id = dev_id;
    wrap->aux.release = kcorp_accel_aux_release;

    ret = kcorp_aux_device_init(&wrap->aux);
    if (ret)
        goto err_free_fw;

    ret = kcorp_aux_device_add(&wrap->aux);
    if (ret)
        goto err_uninit_aux;

    return 0;

err_uninit_aux:
    /* release callback frees both fw_ctx and wrap once initialized. */
    kcorp_aux_device_uninit(&wrap->aux);
    return ret;

err_free_fw:
    kcorp_accel_free_fw_ctx(wrap->fw_ctx);
err_free_wrap:
    kfree(wrap);
    return ret;
}

/* --- Async ring buffer registration with checked multiplication & unwind --- */

#define KCORP_RING_MAX_REG_BUFS 1024u

struct kcorp_iovec_desc {
    u64 base;
    u64 len;
};

struct kcorp_ring_ctx {
    struct mutex reg_lock;
    struct kcorp_iovec_desc *reg_bufs;
    u32 nr_reg_bufs;
};

static inline bool kcorp_check_mul_overflow_size(size_t a, size_t b, size_t *res)
{
    if (a != 0 && b > SIZE_MAX / a)
        return true;
    *res = a * b;
    return false;
}

/**
 * kcorp_ring_register_buffers - Copy and install user buffer descriptors safely
 * @ctx: Ring instance
 * @user_iovs: Userspace pointer to descriptor array
 * @nr_iovs: Number of descriptors requested
 */
int kcorp_ring_register_buffers(struct kcorp_ring_ctx *ctx,
                                const void *user_iovs,
                                u32 nr_iovs)
{
    struct kcorp_iovec_desc *table;
    size_t alloc_bytes = 0;
    u32 i;

    if (!ctx || !user_iovs || nr_iovs == 0 || nr_iovs > KCORP_RING_MAX_REG_BUFS)
        return -EINVAL;

    if (kcorp_check_mul_overflow_size((size_t)nr_iovs, sizeof(*table), &alloc_bytes))
        return -EINVAL;

    table = kmalloc(alloc_bytes, GFP_KERNEL);
    if (!table)
        return -ENOMEM;

    if (copy_from_user(table, user_iovs, alloc_bytes) != 0) {
        kfree(table);
        return -EFAULT;
    }

    for (i = 0; i < nr_iovs; i++) {
        if (table[i].len == 0 || table[i].base > UINT64_MAX - table[i].len) {
            kfree(table);
            return -EINVAL;
        }
    }

    mutex_lock(&ctx->reg_lock);
    kfree(ctx->reg_bufs);
    ctx->reg_bufs = table;
    ctx->nr_reg_bufs = nr_iovs;
    mutex_unlock(&ctx->reg_lock);
    return 0;
}

/* --- Ethernet GRO RX fragment coalescing with explicit bounds flush --- */

void kcorp_napi_gro_flush_skb(struct kcorp_skb_shared_info *shinfo);

/**
 * kcorp_gro_append_rx_frags - Append RX ring segments with MAX_SKB_FRAGS guard
 * @shinfo: Current aggregation skb shared info
 * @frags_in: Incoming hardware fragments
 * @count: Number of incoming fragments
 */
int kcorp_gro_append_rx_frags(struct kcorp_skb_shared_info *shinfo,
                              const struct kcorp_skb_frag *frags_in,
                              unsigned int count)
{
    unsigned int i;

    if (!shinfo || !frags_in)
        return -EINVAL;

    for (i = 0; i < count; i++) {
        if (frags_in[i].size == 0)
            continue;

        if (shinfo->nr_frags >= KCORP_MAX_SKB_FRAGS) {
            kcorp_napi_gro_flush_skb(shinfo);
            shinfo->nr_frags = 0;
            shinfo->data_len = 0;
        }

        shinfo->frags[shinfo->nr_frags] = frags_in[i];
        shinfo->nr_frags++;
        shinfo->data_len += frags_in[i].size;
    }

    return 0;
}
