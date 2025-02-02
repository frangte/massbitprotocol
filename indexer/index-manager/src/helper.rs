use std::{path::PathBuf, env};
use std::error::Error;
use std::fs;
use tokio_compat_02::FutureExt;
use tonic::{Request};
use diesel::{RunQueryDsl, PgConnection, Connection};

// Massbit dependencies
use ipfs_client::core::create_ipfs_clients;
use massbit_chain_substrate::data_type::SubstrateBlock;
use plugin::manager::PluginManager;
use stream_mod::{GenericDataProto, GetBlocksRequest};
use stream_mod::streamout_client::StreamoutClient;
use crate::types::{DeployParams, DeployType};
use index_store::core::IndexStore;
use std::fs::File;
use std::io::{Read};

pub async fn get_index_config(ipfs_config_hash: &String) -> serde_yaml::Mapping {
    let ipfs_addresses = vec!["0.0.0.0:5001".to_string()];
    let ipfs_clients = create_ipfs_clients(&ipfs_addresses).await; // Refactor to use lazy load

    let file_bytes = ipfs_clients[0]
        .cat_all(ipfs_config_hash.to_string())
        .compat()
        .await
        .unwrap()
        .to_vec();

    serde_yaml::from_slice(&file_bytes).unwrap()
}

pub async fn get_index_mapping_file_name(ipfs_mapping_hash: &String) -> String {
    let ipfs_addresses = vec!["0.0.0.0:5001".to_string()];
    let ipfs_clients = create_ipfs_clients(&ipfs_addresses).await; // Refactor to use lazy load

    let file_bytes = ipfs_clients[0]
        .cat_all(ipfs_mapping_hash.to_string())
        .compat()
        .await
        .unwrap()
        .to_vec();

    let file_name = [ipfs_mapping_hash, ".so"].join("");
    let res = fs::write(&file_name, file_bytes); // Add logger and says that write file successfully

    match res {
        Ok(_) => {
            log::info!("[Index Manager Helper] Write SO file to local storage successfully");
            file_name
        },
        Err(err) => {
            panic!("[Index Manager Helper] Could not write file to local storage {:#?}", err)
        }
    }
}

pub async fn get_raw_query_from_model_hash(ipfs_model_hash: &String) -> String {
    log::info!("[Index Manager Helper] Downloading Raw Query from IPFS");
    let ipfs_addresses = vec!["0.0.0.0:5001".to_string()];
    let ipfs_clients = create_ipfs_clients(&ipfs_addresses).await;

    let file_bytes = ipfs_clients[0]
        .cat_all(ipfs_model_hash.to_string())
        .compat()
        .await
        .unwrap()
        .to_vec();

    let raw_query = std::str::from_utf8(&file_bytes).unwrap();
    String::from(raw_query)
}

pub mod stream_mod {
    tonic::include_proto!("chaindata");
}
const URL: &str = "http://127.0.0.1:50051";
pub async fn loop_blocks(params: DeployParams) -> Result<(), Box<dyn Error>> {
    let db_connection_string = match env::var("DATABASE_URL") {
        Ok(connection) => connection,
        Err(_) => String::from("postgres://graph-node:let-me-in@localhost")
    };
    let store = IndexStore {
        connection_string: db_connection_string,
    }; // This store will be used by indexers to insert to database

    // Chain Reader Client to subscribe and get latest block
    let (so_file_path, raw_query) = match params.deploy_type {
        DeployType::Local => {
            // Get SO mapping file location
            let mapping_file_name = params.mapping_path;
            let so_file_path = PathBuf::from(mapping_file_name.to_string());

            // Get raw query to create database
            let mut raw_query = String::new();
            let mut f = File::open(&params.model_path).expect("Unable to open file");
            f.read_to_string(&mut raw_query).expect("Unable to read string");

            (so_file_path, raw_query)
        },
        DeployType::Ipfs => {
            // Get SO mapping file location
            let mapping_file_name = get_index_mapping_file_name(&params.mapping_path).await;
            let mapping_file_location = ["./", &mapping_file_name].join("");

            // Get raw query to create database
            let raw_query = get_raw_query_from_model_hash(&params.model_path).await;
            let so_file_path = PathBuf::from(mapping_file_location.to_string());

            (so_file_path, raw_query)
        },
    };

    let query = diesel::sql_query(raw_query);
    let c = PgConnection::establish(&store.connection_string).expect(&format!("Error connecting to {}", store.connection_string));
    let result = query.execute(&c);
    match result {
        Ok(_) => {
            log::info!("[Index Manager Helper] Table created successfully");
        },
        Err(e) => {
            log::warn!("[Index Manager Helper] {}", e);
        }
    };

    // Chain Reader Client Configuration to subscribe and get latest block from Chain Reader Server
    let mut client = StreamoutClient::connect(URL).await.unwrap();
    let get_blocks_request = GetBlocksRequest{
        start_block_number: 0,
        end_block_number: 1,
    };
    let mut stream = client
        .list_blocks(Request::new(get_blocks_request))
        .await?
        .into_inner();

    // Subscribe new blocks
    // NOTE: Sometimes API doesn't go pass this point. As a hack, we need to create two requests so it will pass.
    while let Some(block) = stream.message().await? {
        let block = block as GenericDataProto;
        log::info!("[Index Manager Helper] Received block = {:?}, hash = {:?} from {:?}",block.block_number, block.block_hash, params.index_name);

        log::info!("[Index Manager Helper] Start plugin manager");

        let mut plugins = PluginManager::new();
        unsafe {
            plugins
                .load(&so_file_path)
                .expect("plugin loading failed");
        }

        let decode_block: SubstrateBlock = serde_json::from_slice(&block.payload).unwrap();
        log::info!("Decoding block: {:?}", decode_block);
        plugins.handle_block(&store, &decode_block); // Call pre-defined handle_block function from plugin manager to start indexing data
    }
    Ok(())
}