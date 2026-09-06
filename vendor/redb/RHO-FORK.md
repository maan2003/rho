# redb 4.2.0, forked for rho

Copied from crates.io `redb-4.2.0` with examples, tests and tooling removed.
One change, in `src/tree_store/page_store/page_manager.rs`,
`allocate_helper_retry`:

The region tracker keeps one "has a free block of this order" bit per region
and order. redb 4.1 cleared those bits only up to the freed page's own order,
so when small frees merged into a large free block the region stayed marked
full for the large orders. 4.2 fixes the free path, but a file written by
4.1 keeps the stale bits, and then `allocate_lowest` never looks at the
empty regions: `Database::compact()` could not move anything down and rho's
store sat at 30 GB with 5 GB in use.

The fork makes the allocator check the regions themselves when the tracker
says no region has a block of the wanted order, and repairs the tracker bit
it finds wrong.
