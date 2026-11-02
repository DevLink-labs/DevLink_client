mod cli;

#[tokio::main]
async fn main() {
    if let Err(error) = cli::run(cli::parse()).await {
        if let Some(exit) = error.downcast_ref::<devlink_cli::setup::terminal::SetupExit>() {
            std::process::exit(i32::from(exit.0));
        }
        eprintln!("DevLink error: {error}");
        std::process::exit(1);
    }
}
