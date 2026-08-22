# Resources

A quick way to spot a runaway process: the Resources tab lists live CPU,
RAM, and GPU usage for every cell that currently has a runner subprocess
in flight.

![Resources tab listing two running cells with CPU, RAM, and GPU usage](../screenshots/resources-overview.png)

Each row is one running cell — its task/cell name, owning squad, and OS
process id, alongside sampled CPU percentage (of one core; can exceed 100%
for multi-threaded work), resident memory, and GPU memory if it could be
attributed to that process (`N/A` when GPU metrics aren't available, e.g. no
`nvidia-smi` on the box). Click any column header to sort by it. **Go to
task →** jumps straight to that exact cell in the [Tasks](tasks.md) tab.

This tab only samples while it's open — CPU sampling briefly blocks to
compute a delta, so it isn't polled in the background on other tabs.
