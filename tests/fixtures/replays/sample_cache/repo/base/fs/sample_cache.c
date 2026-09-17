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
