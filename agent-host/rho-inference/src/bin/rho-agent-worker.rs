#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    if let Err(error) = rho_inference::worker_main() {
        eprintln!("rho-agent-worker: {error:#}");
        std::process::exit(1);
    }
}
