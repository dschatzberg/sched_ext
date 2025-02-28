/* SPDX-License-Identifier: GPL-2.0 */
/*
 * A demo sched_ext user space scheduler which provides vruntime semantics
 * using a simple ordered-list implementation.
 *
 * Each CPU in the system resides in a single, global domain. This precludes
 * the need to do any load balancing between domains. The scheduler could
 * easily be extended to support multiple domains, with load balancing
 * happening in user space.
 *
 * Any task which has any CPU affinity is scheduled entirely in BPF. This
 * program only schedules tasks which may run on any CPU.
 *
 * Copyright (c) 2022 Meta Platforms, Inc. and affiliates.
 * Copyright (c) 2022 Tejun Heo <tj@kernel.org>
 * Copyright (c) 2022 David Vernet <dvernet@meta.com>
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>
#include <sched.h>
#include <signal.h>
#include <assert.h>
#include <libgen.h>
#include <pthread.h>
#include <bpf/bpf.h>
#include <sys/mman.h>
#include <sys/queue.h>
#include <sys/syscall.h>

#include "user_exit_info.h"
#include "scx_userland.h"
#include "scx_userland.skel.h"

const char help_fmt[] =
"A minimal userland sched_ext scheduler.\n"
"\n"
"See the top-level comment in .bpf.c for more details.\n"
"\n"
"Usage: %s [-b BATCH] [-p]\n"
"\n"
"  -b BATCH      The number of tasks to batch when dispatching (default: 8)\n"
"  -p            Don't switch all, switch only tasks on SCHED_EXT policy\n"
"  -h            Display this help and exit\n";

/* Defined in UAPI */
#define SCHED_EXT 7

/* Number of tasks to batch when dispatching to user space. */
static __u32 batch_size = 8;

static volatile int exit_req;
static int enqueued_fd, dispatched_fd;

static struct scx_userland *skel;
static struct bpf_link *ops_link;

/* Stats collected in user space. */
static __u64 nr_vruntime_enqueues, nr_vruntime_dispatches;

/* The data structure containing tasks that are enqueued in user space. */
struct enqueued_task {
	LIST_ENTRY(enqueued_task) entries;
	__u64 sum_exec_runtime;
	double vruntime;
};

/*
 * Use a vruntime-sorted list to store tasks. This could easily be extended to
 * a more optimal data structure, such as an rbtree as is done in CFS. We
 * currently elect to use a sorted list to simplify the example for
 * illustrative purposes.
 */
LIST_HEAD(listhead, enqueued_task);

/*
 * A vruntime-sorted list of tasks. The head of the list contains the task with
 * the lowest vruntime. That is, the task that has the "highest" claim to be
 * scheduled.
 */
static struct listhead vruntime_head = LIST_HEAD_INITIALIZER(vruntime_head);

/*
 * The statically allocated array of tasks. We use a statically allocated list
 * here to avoid having to allocate on the enqueue path, which could cause a
 * deadlock. A more substantive user space scheduler could e.g. provide a hook
 * for newly enabled tasks that are passed to the scheduler from the
 * .prep_enable() callback to allows the scheduler to allocate on safe paths.
 */
struct enqueued_task tasks[USERLAND_MAX_TASKS];

static double min_vruntime;

static void sigint_handler(int userland)
{
	exit_req = 1;
}

static __u32 task_pid(const struct enqueued_task *task)
{
	return ((uintptr_t)task - (uintptr_t)tasks) / sizeof(*task);
}

static int dispatch_task(s32 pid)
{
	int err;

	err = bpf_map_update_elem(dispatched_fd, NULL, &pid, 0);
	if (err) {
		fprintf(stderr, "Failed to dispatch task %d\n", pid);
		exit_req = 1;
	} else {
		nr_vruntime_dispatches++;
	}

	return err;
}

static struct enqueued_task *get_enqueued_task(__s32 pid)
{
	if (pid >= USERLAND_MAX_TASKS)
		return NULL;

	return &tasks[pid];
}

static double calc_vruntime_delta(__u64 weight, __u64 delta)
{
	double weight_f = (double)weight / 100.0;
	double delta_f = (double)delta;

	return delta_f / weight_f;
}

static void update_enqueued(struct enqueued_task *enqueued, const struct scx_userland_enqueued_task *bpf_task)
{
	__u64 delta;

	delta = bpf_task->sum_exec_runtime - enqueued->sum_exec_runtime;

	enqueued->vruntime += calc_vruntime_delta(bpf_task->weight, delta);
	if (min_vruntime > enqueued->vruntime)
		enqueued->vruntime = min_vruntime;
	enqueued->sum_exec_runtime = bpf_task->sum_exec_runtime;
}

static int vruntime_enqueue(const struct scx_userland_enqueued_task *bpf_task)
{
	struct enqueued_task *curr, *enqueued, *prev;

	curr = get_enqueued_task(bpf_task->pid);
	if (!curr)
		return ENOENT;

	update_enqueued(curr, bpf_task);
	nr_vruntime_enqueues++;

	/*
	 * Enqueue the task in a vruntime-sorted list. A more optimal data
	 * structure such as an rbtree could easily be used as well. We elect
	 * to use a list here simply because it's less code, and thus the
	 * example is less convoluted and better serves to illustrate what a
	 * user space scheduler could look like.
	 */

	if (LIST_EMPTY(&vruntime_head)) {
		LIST_INSERT_HEAD(&vruntime_head, curr, entries);
		return 0;
	}

	LIST_FOREACH(enqueued, &vruntime_head, entries) {
		if (curr->vruntime <= enqueued->vruntime) {
			LIST_INSERT_BEFORE(enqueued, curr, entries);
			return 0;
		}
		prev = enqueued;
	}

	LIST_INSERT_AFTER(prev, curr, entries);

	return 0;
}

static void drain_enqueued_map(void)
{
	while (1) {
		struct scx_userland_enqueued_task task;
		int err;

		if (bpf_map_lookup_and_delete_elem(enqueued_fd, NULL, &task))
			return;

		err = vruntime_enqueue(&task);
		if (err) {
			fprintf(stderr, "Failed to enqueue task %d: %s\n",
				task.pid, strerror(err));
			exit_req = 1;
			return;
		}
	}
}

static void dispatch_batch(void)
{
	__u32 i;

	for (i = 0; i < batch_size; i++) {
		struct enqueued_task *task;
		int err;
		__s32 pid;

		task = LIST_FIRST(&vruntime_head);
		if (!task)
			return;

		min_vruntime = task->vruntime;
		pid = task_pid(task);
		LIST_REMOVE(task, entries);
		err = dispatch_task(pid);
		if (err) {
			fprintf(stderr, "Failed to dispatch task %d in %u\n",
				pid, i);
			return;
		}
	}
}

static void *run_stats_printer(void *arg)
{
	while (!exit_req) {
		__u64 nr_failed_enqueues, nr_kernel_enqueues, nr_user_enqueues, total;

		nr_failed_enqueues = skel->bss->nr_failed_enqueues;
		nr_kernel_enqueues = skel->bss->nr_kernel_enqueues;
		nr_user_enqueues = skel->bss->nr_user_enqueues;
		total = nr_failed_enqueues + nr_kernel_enqueues + nr_user_enqueues;

		printf("o-----------------------o\n");
		printf("| BPF ENQUEUES          |\n");
		printf("|-----------------------|\n");
		printf("|  kern:     %10llu |\n", nr_kernel_enqueues);
		printf("|  user:     %10llu |\n", nr_user_enqueues);
		printf("|  failed:   %10llu |\n", nr_failed_enqueues);
		printf("|  -------------------- |\n");
		printf("|  total:    %10llu |\n", total);
		printf("|                       |\n");
		printf("|-----------------------|\n");
		printf("| VRUNTIME / USER       |\n");
		printf("|-----------------------|\n");
		printf("|  enq:      %10llu |\n", nr_vruntime_enqueues);
		printf("|  disp:     %10llu |\n", nr_vruntime_dispatches);
		printf("o-----------------------o\n");
		printf("\n\n");
		sleep(1);
	}

	return NULL;
}

static int spawn_stats_thread(void)
{
	pthread_t stats_printer;

	return pthread_create(&stats_printer, NULL, run_stats_printer, NULL);
}

static int bootstrap(int argc, char **argv)
{
	int err;
	__u32 opt;
	struct sched_param sched_param = {
		.sched_priority = sched_get_priority_max(SCHED_EXT),
	};
	bool switch_partial = false;

	signal(SIGINT, sigint_handler);
	signal(SIGTERM, sigint_handler);
	libbpf_set_strict_mode(LIBBPF_STRICT_ALL);

	/*
	 * Enforce that the user scheduler task is managed by sched_ext. The
	 * task eagerly drains the list of enqueued tasks in its main work
	 * loop, and then yields the CPU. The BPF scheduler only schedules the
	 * user space scheduler task when at least one other task in the system
	 * needs to be scheduled.
	 */
	err = syscall(__NR_sched_setscheduler, getpid(), SCHED_EXT, &sched_param);
	if (err) {
		fprintf(stderr, "Failed to set scheduler to SCHED_EXT: %s\n", strerror(err));
		return err;
	}

	while ((opt = getopt(argc, argv, "b:ph")) != -1) {
		switch (opt) {
		case 'b':
			batch_size = strtoul(optarg, NULL, 0);
			break;
		case 'p':
			switch_partial = true;
			break;
		default:
			fprintf(stderr, help_fmt, basename(argv[0]));
			exit(opt != 'h');
		}
	}

	/*
	 * It's not always safe to allocate in a user space scheduler, as an
	 * enqueued task could hold a lock that we require in order to be able
	 * to allocate.
	 */
	err = mlockall(MCL_CURRENT | MCL_FUTURE);
	if (err) {
		fprintf(stderr, "Failed to prefault and lock address space: %s\n",
			strerror(err));
		return err;
	}

	skel = scx_userland__open();
	if (!skel) {
		fprintf(stderr, "Failed to open scheduler: %s\n", strerror(errno));
		return errno;
	}
	skel->rodata->num_possible_cpus = libbpf_num_possible_cpus();
	assert(skel->rodata->num_possible_cpus > 0);
	skel->rodata->usersched_pid = getpid();
	assert(skel->rodata->usersched_pid > 0);
	skel->rodata->switch_partial = switch_partial;

	err = scx_userland__load(skel);
	if (err) {
		fprintf(stderr, "Failed to load scheduler: %s\n", strerror(err));
		goto destroy_skel;
	}

	enqueued_fd = bpf_map__fd(skel->maps.enqueued);
	dispatched_fd = bpf_map__fd(skel->maps.dispatched);
	assert(enqueued_fd > 0);
	assert(dispatched_fd > 0);

	err = spawn_stats_thread();
	if (err) {
		fprintf(stderr, "Failed to spawn stats thread: %s\n", strerror(err));
		goto destroy_skel;
	}

	ops_link = bpf_map__attach_struct_ops(skel->maps.userland_ops);
	if (!ops_link) {
		fprintf(stderr, "Failed to attach struct ops: %s\n", strerror(errno));
		err = errno;
		goto destroy_skel;
	}

	return 0;

destroy_skel:
	scx_userland__destroy(skel);
	exit_req = 1;
	return err;
}

static void sched_main_loop(void)
{
	while (!exit_req) {
		/*
		 * Perform the following work in the main user space scheduler
		 * loop:
		 *
		 * 1. Drain all tasks from the enqueued map, and enqueue them
		 *    to the vruntime sorted list.
		 *
		 * 2. Dispatch a batch of tasks from the vruntime sorted list
		 *    down to the kernel.
		 *
		 * 3. Yield the CPU back to the system. The BPF scheduler will
		 *    reschedule the user space scheduler once another task has
		 *    been enqueued to user space.
		 */
		drain_enqueued_map();
		dispatch_batch();
		sched_yield();
	}
}

int main(int argc, char **argv)
{
	int err;

	err = bootstrap(argc, argv);
	if (err) {
		fprintf(stderr, "Failed to bootstrap scheduler: %s\n", strerror(err));
		return err;
	}

	sched_main_loop();

	exit_req = 1;
	bpf_link__destroy(ops_link);
	uei_print(&skel->bss->uei);
	scx_userland__destroy(skel);
	return 0;
}
