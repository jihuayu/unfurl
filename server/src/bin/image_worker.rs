fn main() -> Result<(), Box<dyn std::error::Error>> {
    unfurl_server::image_worker::run_worker()
}
