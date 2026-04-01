// Example usage of OpencodeRunner::model() to list configured Klava models
//
// This example demonstrates how to retrieve all Klava models from OpenCode's
// configuration file (~/.config/opencode/opencode.json)
//
// Run with: cargo run --example list_klava_models --features opencode
extern crate klava;
#[cfg(feature = "opencode")]
use klava::agents::{AgentRunner, OpencodeRunner};

#[cfg(feature = "opencode")]
fn main() -> anyhow::Result<()> {
    println!("Fetching Klava models from OpenCode configuration...");
    println!();

    let runner = OpencodeRunner::new();
    let models = runner.model();

    if OpencodeRunner::check_installation().is_err() {
        println!("No OpenCode configuration found.");
    }

    if models.is_empty() {
        println!("No Klava models found in OpenCode configuration.");
        println!();
        println!("To add Klava models, edit ~/.config/opencode/opencode.json and add");
        println!("models with the '_klava' flag or names ending with ' [Klava]'");
        return Ok(());
    }

    println!("Found {} Klava model(s):", models.len());
    println!();

    for (index, model) in models.iter().enumerate() {
        println!("{:2}. {}", index + 1, model);
    }

    Ok(())
}

#[cfg(not(feature = "opencode"))]
fn main() -> anyhow::Result<()> {
    eprintln!("This example requires the 'opencode' feature to be enabled.");
    eprintln!("Run with: cargo run --example list_klava_models --features opencode");
    std::process::exit(1);
}
