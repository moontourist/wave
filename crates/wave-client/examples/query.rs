use std::error::Error;

use wave_client::WaveMachClient;

fn main() -> Result<(), Box<dyn Error>> {
    let client = WaveMachClient::connect()?;
    let data = client.get_workspaces(None)?;

    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}
