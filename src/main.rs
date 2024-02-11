use redis_rover::errors;
use redis_rover::termination::{create_termination, Interrupted};

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    errors::install_hooks()?;

    let (_terminator, mut interrupt_rx) = create_termination();

    if let Ok(reason) = interrupt_rx.recv().await {
        match reason {
            Interrupted::UserInt => println!("exited per user request"),
            Interrupted::OsSigInt => println!("exited because of an os sig int"),
        }
    }
    Ok(())
}
