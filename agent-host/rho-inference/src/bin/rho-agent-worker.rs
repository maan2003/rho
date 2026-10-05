#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[unsafe(export_name = "malloc_conf")]
static MALLOC_CONF: &[u8; 27] = rho_agent::heap::MALLOC_CONF;

fn main() {
    if let Err(error) = rho_inference::worker_main() {
        eprintln!("rho-agent-worker: {error:#}");
        std::process::exit(1);
    }
}
