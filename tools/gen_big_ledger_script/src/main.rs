use std::{collections::HashMap, path::PathBuf};

use clap::{arg, command, Parser};
use serde::Deserialize;

use massa_api_exports::operation::OperationInput;
// pub struct OperationInput {
//     /// The public key of the creator of the TX
//     pub creator_public_key: PublicKey,
//     /// The signature of the operation
//     pub signature: Signature,
//     /// The serialized version of the content `base58` encoded
//     pub serialized_content: Vec<u8>,
// }

use massa_sdk::Client;

use crate::opcreator::OperationCreator;
// impl Client {
//     /// creates a new client
//     pub async fn new(
//         ip: IpAddr,
//         public_port: u16,
//         private_port: u16,
//         grpc_public_port: u16,
//         grpc_private_port: u16,
//         http_config: &HttpConfig,
//     ) -> Result<Client, ClientError> {

// pub async fn send_operations(
//     &self,
//     operations: Vec<OperationInput>,
// ) -> RpcResult<Vec<OperationId>> {

mod opcreator;

#[derive(Deserialize, Debug)]
pub struct ServerData {
    ip: String,
    node_privkey: String,
    node_pubkey: String,
    address: String,
    bootstrap_server: bool,
    inject_operations: usize,
}

#[derive(Deserialize, Debug)]
pub struct DeployData {
    servers: HashMap<String, ServerData>,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short='d', long)]
    deploy_file: PathBuf,

    #[arg(short='a', long)]
    api_node_host: String,
}

fn main() {
    let args = Args::parse();
    let deploy_data: DeployData = toml::from_str(
        std::fs::read_to_string(&args.deploy_file)
            .expect("Unable to read deploy file")
            .as_str(),
    )
    .expect("Unable to load data from deploy file data");

    println!("{:?}", deploy_data);

    assert!(deploy_data.servers.len() > 1, "Not enough serverse are configured");

    let op_creator = OperationCreator::from_data(&deploy_data);
}
