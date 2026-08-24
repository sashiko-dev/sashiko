#ifndef _LINUX_SAMPLE_CACHE_H
#define _LINUX_SAMPLE_CACHE_H

#include <linux/types.h>
#include <linux/list.h>

struct sample_cache_entry {
	struct list_head list;
	u64 id;
	void *data;
};

void sample_cache_init(void);

#endif /* _LINUX_SAMPLE_CACHE_H */
