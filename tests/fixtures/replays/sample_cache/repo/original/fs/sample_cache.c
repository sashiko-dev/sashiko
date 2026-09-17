// SPDX-License-Identifier: GPL-2.0
#include <linux/module.h>
#include <linux/types.h>
#include <linux/spinlock.h>
#include <linux/slab.h>
#include <linux/sample_cache.h>

static DEFINE_SPINLOCK(sample_cache_lock);
static LIST_HEAD(sample_cache_list);

void sample_cache_init(void)
{
	INIT_LIST_HEAD(&sample_cache_list);
}

int sample_cache_insert(struct sample_cache_entry *entry)
{
	if (!entry)
		return -EINVAL;

	spin_lock(&sample_cache_lock);
	list_add_tail(&entry->list, &sample_cache_list);
	spin_unlock(&sample_cache_lock);
	return 0;
}

void sample_cache_purge(void)
{
	struct sample_cache_entry *entry, *tmp;

	spin_lock(&sample_cache_lock);
	list_for_each_entry_safe(entry, tmp, &sample_cache_list, list) {
		list_del(&entry->list);
		kfree(entry);
	}
	spin_unlock(&sample_cache_lock);
}
