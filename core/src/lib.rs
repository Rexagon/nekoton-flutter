mod external;
mod ffi;
mod native_signer;

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_longlong, c_uchar, c_uint};
use std::sync::Arc;

use anyhow::Result;
use ed25519_dalek::PublicKey;
use tokio::sync::RwLock;

use nekoton::core::models::{AccountState, PendingTransaction, Transaction, TransactionsBatchInfo};
use nekoton::core::ton_wallet;
use nekoton::transport::gql;
use nekoton::transport::Transport;

use crate::external::GqlConnection;
use crate::ffi::IntoDart;

pub struct CoreState {}

pub struct Runtime {
    inner: Arc<tokio::runtime::Runtime>,
}

impl Runtime {
    pub fn new(worker_threads: usize) -> Result<Self> {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .enable_all()
                .build()?,
        );

        std::thread::spawn({
            let runtime = runtime.clone();
            move || {
                runtime.block_on(async move {
                    futures::future::pending::<()>().await;
                });
            }
        });

        Ok(Self { inner: runtime })
    }
}

pub struct TonWallet {
    transport: Arc<dyn Transport>,
    wallet: Arc<RwLock<ton_wallet::TonWallet>>,
}

#[no_mangle]
pub unsafe extern "C" fn init(post_cobject: ffi::DartPostCObjectFnType) {
    ffi::POST_COBJECT = Some(post_cobject);
}

#[repr(C)]
pub struct RuntimeParams {
    pub worker_threads: c_uint,
}

#[no_mangle]
pub unsafe extern "C" fn create_runtime(
    params: RuntimeParams,
    runtime: *mut *const Runtime,
) -> ExitCode {
    if runtime.is_null() {
        return ExitCode::FailedToCreateRuntime;
    }

    match Runtime::new(params.worker_threads as usize) {
        Ok(new_runtime) => {
            *runtime = Box::into_raw(Box::new(new_runtime));
            ExitCode::Ok
        }
        Err(_) => ExitCode::FailedToCreateRuntime,
    }
}

#[no_mangle]
pub unsafe extern "C" fn delete_runtime(runtime: *mut Runtime) -> ExitCode {
    if runtime.is_null() {
        return ExitCode::RuntimeIsNotInitialized;
    }
    Box::from_raw(runtime);
    ExitCode::Ok
}

#[no_mangle]
pub unsafe extern "C" fn wait(
    runtime: *mut Runtime,
    seconds: c_uint,
    send_port: c_longlong,
) -> ExitCode {
    if runtime.is_null() {
        return ExitCode::RuntimeIsNotInitialized;
    }

    (*runtime).inner.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(seconds as u64)).await;

        ffi::SendPort::new(send_port).post(());
    });

    ExitCode::Ok
}

pub struct GqlTransport {
    inner: Arc<gql::GqlTransport>,
}

impl GqlTransport {
    pub fn new(connection: GqlConnection) -> Self {
        Self {
            inner: Arc::new(gql::GqlTransport::new(Arc::new(connection))),
        }
    }
}

#[repr(C)]
pub struct TransportParams {
    pub url: *mut c_char,
}

#[no_mangle]
pub unsafe extern "C" fn create_gql_transport(
    params: TransportParams,
    gql_transport: *mut *const GqlTransport,
) -> ExitCode {
    let url = match CStr::from_ptr(params.url).to_str() {
        Ok(url) => url,
        Err(_) => return ExitCode::InvalidUrl,
    };

    match GqlConnection::new(url) {
        Ok(connection) => {
            *gql_transport = Box::into_raw(Box::new(GqlTransport::new(connection)));
            ExitCode::Ok
        }
        Err(_) => ExitCode::InvalidUrl,
    }
}

#[no_mangle]
pub unsafe extern "C" fn delete_gql_transport(gql_transport: *mut GqlTransport) -> ExitCode {
    if gql_transport.is_null() {
        return ExitCode::TransportIsNotInitialized;
    }
    Box::from_raw(gql_transport);
    ExitCode::Ok
}

#[no_mangle]
pub unsafe extern "C" fn subscribe_to_ton_wallet(
    runtime: *mut Runtime,
    gql_transport: *mut GqlTransport,
    public_key: *const c_char,
    contract_type: ContractType,
    subscription_port: c_longlong,
    result_port: c_longlong,
) -> ExitCode {
    if runtime.is_null() {
        return ExitCode::RuntimeIsNotInitialized;
    }
    if gql_transport.is_null() {
        return ExitCode::TransportIsNotInitialized;
    }

    let public_key = match read_public_key(public_key) {
        Ok(key) => key,
        Err(_) => return ExitCode::InvalidPublicKey,
    };
    let contract_type = contract_type.into();

    let handler = Arc::new(TonWalletSubscriptionHandler::new(subscription_port));
    let result_port = ffi::SendPort::new(result_port);

    let transport = (*gql_transport).inner.clone();

    (*runtime).inner.spawn(async move {
        match ton_wallet::TonWallet::subscribe(transport, public_key, contract_type, handler).await
        {
            Ok(new_subscription) => {
                let subscription = Box::into_raw(Box::new(TonWalletSubscription {
                    inner: new_subscription,
                }));

                result_port.post((ExitCode::Ok, subscription));
            }
            Err(_) => {
                result_port.post((
                    ExitCode::FailedToSubscribeToTonWallet,
                    std::ptr::null::<TonWalletSubscription>(),
                ));
            }
        }
    });

    ExitCode::Ok
}

#[no_mangle]
pub unsafe extern "C" fn delete_subscription(subscription: *mut TonWalletSubscription) -> ExitCode {
    if subscription.is_null() {
        return ExitCode::SubscriptionIsNotInitialized;
    }
    Box::from_raw(subscription);
    ExitCode::Ok
}

pub struct TonWalletSubscription {
    inner: ton_wallet::TonWallet,
}

struct TonWalletSubscriptionHandler {
    port: ffi::SendPort,
}

impl TonWalletSubscriptionHandler {
    pub fn new(port: i64) -> Self {
        Self {
            port: ffi::SendPort::new(port),
        }
    }
}

impl ton_wallet::TonWalletSubscriptionHandler for TonWalletSubscriptionHandler {
    fn on_message_sent(
        &self,
        _pending_transaction: PendingTransaction,
        _transaction: Option<Transaction>,
    ) {
        // TODO
    }

    fn on_message_expired(&self, _pending_transaction: PendingTransaction) {
        // TODO
    }

    fn on_state_changed(&self, new_state: AccountState) {
        self.port.post(new_state.balance);
    }

    fn on_transactions_found(
        &self,
        _transactions: Vec<Transaction>,
        _batch_info: TransactionsBatchInfo,
    ) {
        // TODO
    }
}

fn read_public_key(public_key: *const c_char) -> Result<PublicKey> {
    if public_key.is_null() {
        return Err(NekotonError::NullPointerPassed.into());
    }

    let public_key = unsafe { CStr::from_ptr(public_key) }.to_str()?;
    let data = hex::decode(public_key)?;
    let public_key = PublicKey::from_bytes(&data)?;
    Ok(public_key)
}

#[repr(C)]
pub enum ContractType {
    SafeMultisig,
    SafeMultisig24h,
    SetcodeMultisig,
    Surf,
    WalletV3,
}

impl From<ContractType> for ton_wallet::ContractType {
    fn from(t: ContractType) -> Self {
        match t {
            ContractType::SafeMultisig => {
                ton_wallet::ContractType::Multisig(ton_wallet::MultisigType::SafeMultisigWallet)
            }
            ContractType::SafeMultisig24h => {
                ton_wallet::ContractType::Multisig(ton_wallet::MultisigType::SafeMultisigWallet24h)
            }
            ContractType::SetcodeMultisig => {
                ton_wallet::ContractType::Multisig(ton_wallet::MultisigType::SetcodeMultisigWallet)
            }
            ContractType::Surf => {
                ton_wallet::ContractType::Multisig(ton_wallet::MultisigType::SurfWallet)
            }
            ContractType::WalletV3 => ton_wallet::ContractType::WalletV3,
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum NekotonError {
    #[error("Null pointer passed")]
    NullPointerPassed,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub enum ExitCode {
    Ok = 0,

    FailedToCreateRuntime,
    RuntimeIsNotInitialized,
    TransportIsNotInitialized,
    SubscriptionIsNotInitialized,
    FailedToSubscribeToTonWallet,

    InvalidUrl,
    InvalidPublicKey,
}

impl IntoDart for ExitCode {
    fn into_dart(self) -> ffi::DartCObject {
        (self as c_int).into_dart()
    }
}
