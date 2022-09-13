use std::{borrow::Borrow, path::Path};

use contract_transcode::ContractMessageTranscoder;
use ink_metadata::{InkProject, MetadataVersion};

use jsonschema::JSONSchema;
use once_cell::sync::Lazy;
use pallet_contracts_primitives::{ContractResult, ExecReturnValue, GetStorageResult};
use parity_scale_codec::{Decode, Encode};

use sp_core::{crypto::AccountId32, hexdisplay::AsBytesRef, Bytes};
use subxt::{
    ext::sp_runtime::DispatchError,
    rpc::{rpc_params, ClientT},
    tx::{PairSigner, TxEvents},
    Config, OnlineClient, PolkadotConfig,
};

use contract_metadata::ContractMetadata;
use sp_keyring::AccountKeyring;
use tokio::time::timeout;

mod cases;

// metadata file obtained from the latest substrate-contracts-node
#[subxt::subxt(runtime_metadata_path = "./metadata.scale")]
pub mod node {}

pub type API = OnlineClient<PolkadotConfig>;

pub struct DeployContract {
    pub caller: AccountKeyring,
    pub selector: Vec<u8>,
    pub value: u128,
    pub code: Vec<u8>,
}
pub struct WriteContract {
    pub caller: AccountKeyring,
    pub contract_address: AccountId32,
    pub selector: Vec<u8>,
    pub value: u128,
}
pub struct ReadContract {
    pub caller: AccountKeyring,
    pub contract_address: AccountId32,
    pub value: u128,
    pub selector: Vec<u8>,
}

pub struct ReadLayout {
    pub contract_address: AccountId32,
    pub key: Vec<u8>,
}

#[async_trait::async_trait]
trait Execution {
    type Output;

    async fn execute(self, api: &API) -> Result<Self::Output, anyhow::Error>;
}

pub mod output {
    use super::*;
    pub struct Deployed {
        pub contract_address: AccountId32,
        pub events: Vec<node::contracts::events::ContractEmitted>,
    }
    pub struct WriteSuccess {
        pub events: Vec<node::contracts::events::ContractEmitted>,
    }
    pub struct ReadSuccess {
        pub return_value: Vec<u8>,
    }
}

const GAS_LIMIT: u64 = 2 * 10_u64.pow(11);

fn random_salt() -> Vec<u8> {
    let random_u8 = rand::random::<[u8; 32]>();
    Bytes::from(random_u8.to_vec()).encode()
}

#[async_trait::async_trait]
impl Execution for DeployContract {
    type Output = output::Deployed;

    async fn execute(self, api: &API) -> Result<Self::Output, anyhow::Error> {
        let Self {
            caller,
            selector,
            code,
            value,
        } = self;

        let evts = raw_instantiate_and_upload(
            api,
            caller,
            value,
            GAS_LIMIT,
            None,
            code,
            selector,
            random_salt(),
        )
        .await?;

        let contract_address = evts
            .iter()
            .find_map(|e| {
                e.ok()
                    .and_then(|i| i.as_event::<node::contracts::events::Instantiated>().ok())
                    .flatten()
                    .map(|i| i.contract)
            })
            .ok_or_else(|| anyhow::anyhow!("unable to find deployed"))?;

        let events = evts
            .iter()
            .filter_map(|e| {
                e.ok()
                    .and_then(|v| {
                        v.as_event::<node::contracts::events::ContractEmitted>()
                            .ok()
                    })
                    .flatten()
            })
            .collect::<Vec<_>>();

        Ok(output::Deployed {
            contract_address,
            events,
        })
    }
}

#[async_trait::async_trait]
impl Execution for WriteContract {
    type Output = output::WriteSuccess;

    async fn execute(self, api: &API) -> Result<Self::Output, anyhow::Error> {
        let Self {
            caller,
            contract_address,
            selector,
            value,
        } = self;

        let evts = raw_call(
            api,
            contract_address,
            caller,
            value,
            GAS_LIMIT,
            None,
            selector,
        )
        .await?;

        if let Some(e) = evts.iter().filter_map(|e| e.ok()).find_map(|e| {
            e.as_event::<node::system::events::ExtrinsicFailed>()
                .ok()
                .flatten()
        }) {
            if let node::runtime_types::sp_runtime::DispatchError::Module(e) = &e.dispatch_error {
                if let Ok(details) = api.metadata().error(e.index, e.error[0]) {
                    return Err(anyhow::anyhow!("{details:?}"));
                }
            }

            return Err(anyhow::anyhow!("{e:?}"));
        }

        let events = evts
            .iter()
            .filter_map(|e| {
                e.ok()
                    .and_then(|v| {
                        v.as_event::<node::contracts::events::ContractEmitted>()
                            .ok()
                    })
                    .flatten()
            })
            .collect::<Vec<_>>();

        Ok(output::WriteSuccess { events })
    }
}

#[async_trait::async_trait]
impl Execution for ReadContract {
    type Output = output::ReadSuccess;

    async fn execute(self, api: &API) -> Result<Self::Output, anyhow::Error> {
        let Self {
            caller,
            contract_address,
            selector,
            value,
        } = self;

        let rv = read_call(api, caller, contract_address, value, selector).await?;

        if rv.did_revert() {
            Err(anyhow::anyhow!("reverted"))
        } else {
            Ok(output::ReadSuccess {
                return_value: rv.data.to_vec(),
            })
        }
    }
}

#[async_trait::async_trait]
impl Execution for ReadLayout {
    type Output = GetStorageResult;

    async fn execute(self, api: &API) -> Result<Self::Output, anyhow::Error> {
        let ReadLayout {
            contract_address,
            key,
        } = self;

        query_call(api, contract_address, key).await
    }
}

#[derive(Encode)]
pub struct CallRequest {
    origin: <PolkadotConfig as Config>::AccountId,
    dest: <PolkadotConfig as Config>::AccountId,
    value: u128,
    gas_limit: u64,
    storage_deposit_limit: Option<u128>,
    input_data: Vec<u8>,
}

async fn raw_instantiate_and_upload(
    api: &API,
    builtin_keyring: sp_keyring::AccountKeyring,
    value: u128,
    gas_limit: u64,
    storage_deposit_limit: Option<u128>,
    code: Vec<u8>,
    data: Vec<u8>,
    salt: Vec<u8>,
) -> anyhow::Result<TxEvents<PolkadotConfig>> {
    let signer = PairSigner::new(builtin_keyring.pair());

    let payload = node::tx().contracts().instantiate_with_code(
        value,
        gas_limit,
        storage_deposit_limit,
        code,
        data,
        salt,
    );

    let evt = api
        .tx()
        .sign_and_submit_then_watch_default(&payload, &signer)
        .await?
        .wait_for_in_block()
        .await?
        .fetch_events()
        .await?;

    Ok(evt)
}

async fn raw_upload(
    api: &API,
    builtin_keyring: sp_keyring::AccountKeyring,
    storage_deposit_limit: Option<u128>,
    code: Vec<u8>,
) -> anyhow::Result<TxEvents<PolkadotConfig>> {
    let signer = PairSigner::new(builtin_keyring.pair());

    let payload = node::tx().contracts().upload_code(code, None);

    let evt = api
        .tx()
        .sign_and_submit_then_watch_default(&payload, &signer)
        .await?
        .wait_for_in_block()
        .await?
        .fetch_events()
        .await?;

    Ok(evt)
}

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn raw_call(
    api: &API,
    dest: AccountId32,
    builtin_keyring: sp_keyring::AccountKeyring,
    value: u128,
    gas_limit: u64,
    storage_deposit_limit: Option<u128>,
    data: Vec<u8>,
) -> anyhow::Result<TxEvents<PolkadotConfig>> {
    let signer = PairSigner::new(builtin_keyring.pair());

    let payload = node::tx().contracts().call(
        subxt::ext::sp_runtime::MultiAddress::Id(dest),
        value,
        gas_limit,
        storage_deposit_limit,
        data,
    );

    let evt = timeout(
        TIMEOUT,
        api.tx()
            .sign_and_submit_then_watch_default(&payload, &signer)
            .await?
            .wait_for_in_block()
            .await?
            .fetch_events(),
    )
    .await??;

    Ok(evt)
}

async fn query_call(
    api: &API,
    contract_address: AccountId32,
    key: Vec<u8>,
) -> anyhow::Result<GetStorageResult> {
    let params = rpc_params![
        "ContractsApi_get_storage",
        Bytes((contract_address, key).encode())
    ];
    let rv: Bytes = api.rpc().client.request("state_call", params).await?;

    <GetStorageResult>::decode(&mut rv.as_bytes_ref()).map_err(|e| anyhow::anyhow!("{e:?}"))
}

async fn read_call(
    api: &API,
    caller: AccountKeyring,
    contract_address: AccountId32,
    value: u128,
    selector: Vec<u8>,
) -> anyhow::Result<ExecReturnValue> {
    let req = CallRequest {
        origin: caller.into(),
        dest: contract_address,
        value,
        gas_limit: GAS_LIMIT,
        storage_deposit_limit: None,
        input_data: selector,
    };

    let params = rpc_params!["ContractsApi_call", Bytes(req.encode())];
    let rv: Bytes = api.rpc().client.request("state_call", params).await?;

    let rv = ContractResult::<Result<ExecReturnValue, DispatchError>, u128>::decode(
        &mut rv.as_bytes_ref(),
    )?;

    rv.result.map_err(|e| {
        if let DispatchError::Module(m) = e {
            if let Ok(d) = api.metadata().error(m.index, m.error) {
                return anyhow::anyhow!("{d:?}");
            }
        }

        anyhow::anyhow!("{e:?}")
    })
}

// static SCHEMA: Lazy<JSONSchema> = Lazy::new(|| {
//     let raw = include_bytes!("../ink-v3-schema.json");
//     let val: serde_json::Value = serde_json::from_slice(raw).unwrap();

//     JSONSchema::compile(&val).unwrap()
// });

fn load_versioned_metadata(contract: &ContractMetadata) -> anyhow::Result<InkProject> {
    let abi_json = serde_json::Value::Object(contract.abi.clone());

    // let schema = SCHEMA.borrow();
    // assert!(schema.is_valid(&abi_json));

    let project = serde_json::from_value::<InkProject>(abi_json).unwrap();
    Ok(project)
}

pub fn load_project(path: impl AsRef<Path>) -> anyhow::Result<InkProject> {
    let r = std::fs::File::open(path)?;

    let contract: ContractMetadata = serde_json::from_reader(r)?;

    load_versioned_metadata(&contract)
}

pub async fn free_balance_of(api: &API, addr: AccountId32) -> anyhow::Result<u128> {
    let key = node::storage().system().account(addr);

    let val = api.storage().fetch_or_default(&key, None).await?;

    Ok(val.data.free)
}

struct Contract {
    path: &'static str,
    transcoder: ContractMessageTranscoder,
    blob: Vec<u8>,
    address: Option<AccountId32>,
}

impl Contract {
    pub fn new(path: &'static str) -> anyhow::Result<Self> {
        let r = std::fs::File::open(path)?;

        let contract: ContractMetadata = serde_json::from_reader(r)?;
        let project = load_versioned_metadata(&contract)?;

        let transcoder = ContractMessageTranscoder::new(project);

        let blob = contract
            .source
            .wasm
            .map(|v| v.0)
            .expect("unable to find wasm blob");

        Ok(Self {
            path,
            transcoder,
            blob,
            address: None,
        })
    }

    pub fn from_addr(&self, address: AccountId32) -> anyhow::Result<Self> {
        let mut out = Contract::new(self.path)?;

        out.address.replace(address);

        Ok(out)
    }

    pub async fn upload_code(
        &self,
        api: &API,
        caller: sp_keyring::AccountKeyring,
    ) -> anyhow::Result<()> {
        raw_upload(api, caller, None, self.blob.clone()).await?;

        Ok(())
    }

    pub async fn deploy(
        &mut self,
        api: &API,
        caller: sp_keyring::AccountKeyring,
        value: u128,
        build_selector: impl Fn(&ContractMessageTranscoder) -> Vec<u8>,
    ) -> anyhow::Result<Vec<node::contracts::events::ContractEmitted>> {
        let transcoder = &self.transcoder;

        let selector = build_selector(transcoder);

        let deployed = DeployContract {
            caller,
            selector,
            value,
            code: self.blob.clone(),
        }
        .execute(api)
        .await?;
        let addr = deployed.contract_address;

        self.address.replace(addr.clone());

        Ok(deployed.events)
    }

    pub async fn call(
        &self,
        api: &API,
        caller: sp_keyring::AccountKeyring,
        value: u128,
        build_selector: impl Fn(&ContractMessageTranscoder) -> Vec<u8>,
    ) -> anyhow::Result<Vec<node::contracts::events::ContractEmitted>> {
        let transcoder = &self.transcoder;

        let selector = build_selector(transcoder);

        let out = WriteContract {
            caller,
            selector,
            value,
            contract_address: self.address.clone().unwrap(),
        }
        .execute(api)
        .await?;

        Ok(out.events)
    }

    pub async fn try_call(
        &self,
        api: &API,
        caller: sp_keyring::AccountKeyring,
        value: u128,
        build_selector: impl Fn(&ContractMessageTranscoder) -> Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let transcoder = &self.transcoder;
        let selector = build_selector(transcoder);

        let out = ReadContract {
            caller,
            selector,
            value,
            contract_address: self.address.clone().unwrap(),
        }
        .execute(api)
        .await?;

        Ok(out.return_value)
    }

    pub async fn read_storage(&self, api: &API, key: Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
        let out = ReadLayout {
            contract_address: self.address.clone().unwrap(),
            key,
        }
        .execute(api)
        .await?
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;

        Ok(out)
    }
}
