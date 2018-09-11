use action::{Action, ActionWrapper};
use context::Context;
use error::HolochainError;
use holochain_dna::zome::capabilities::Membrane;
use instance::{Observer, RECV_DEFAULT_TIMEOUT_MS};
use nucleus::{
    launch_zome_fn_call,
    ribosome::api::{runtime_allocate_encode_str, runtime_args_to_utf8, HcApiReturnCode, Runtime},
    state::NucleusState,
    ZomeFnCall,
};
use serde_json;
use std::sync::{
    mpsc::{channel, Sender},
    Arc,
};
use wasmi::{RuntimeArgs, RuntimeValue, Trap};

/// Struct for input data received when Call API function is invoked
#[derive(Deserialize, Default, Clone, PartialEq, Eq, Hash, Debug, Serialize)]
pub struct ZomeCallArgs {
    pub zome_name: String,
    pub cap_name: String,
    pub fn_name: String,
    pub fn_args: String,
}

// ZomeCallArgs to ZomeFnCall
impl ZomeFnCall {
    fn from_args(args: ZomeCallArgs) -> Self {
        ZomeFnCall::new(
            &args.zome_name,
            &args.cap_name,
            &args.fn_name,
            &args.fn_args,
        )
    }
}

/// HcApiFuncIndex::CALL function code
/// args: [0] encoded MemoryAllocation as u32
/// expected complex argument: {zome_name: String, cap_name: String, fn_name: String, args: String}
/// args from API call are converted into a ZomeFnCall
/// Launch an Action::Call with newly formed ZomeFnCall
/// Waits for a ZomeFnResult
/// Returns an HcApiReturnCode as I32
pub fn invoke_call(
    runtime: &mut Runtime,
    args: &RuntimeArgs,
) -> Result<Option<RuntimeValue>, Trap> {
    // deserialize args
    let args_str = runtime_args_to_utf8(&runtime, &args);
    let input: ZomeCallArgs = match serde_json::from_str(&args_str) {
        Ok(input) => input,
        // Exit on error
        Err(_) => {
            // Return Error code in i32 format
            return Ok(Some(RuntimeValue::I32(HcApiReturnCode::ErrorJson as i32)));
        }
    };

    // ZomeCallArgs to ZomeFnCall
    let zome_call = ZomeFnCall::from_args(input);

    // Don't allow recursive calls
    if zome_call.same_as(&runtime.zome_call) {
        return Ok(Some(RuntimeValue::I32(
            HcApiReturnCode::ErrorRecursiveCall as i32,
        )));
    }

    // Create Call Action
    let action_wrapper = ActionWrapper::new(Action::Call(zome_call.clone()));
    // Send Action and block
    let (sender, receiver) = channel();
    ::instance::dispatch_action_with_observer(
        &runtime.action_channel,
        &runtime.observer_channel,
        action_wrapper.clone(),
        move |state: &::state::State| {
            // Observer waits for a ribosome_call_result
            let maybe_result = state.nucleus().zome_call_result(&zome_call);
            match maybe_result {
                Some(result) => {
                    // @TODO never panic in wasm
                    // @see https://github.com/holochain/holochain-rust/issues/159
                    sender
                        .send(result)
                        // the channel stays connected until the first message has been sent
                        // if this fails that means that it was called after having returned done=true
                        .expect("observer called after done");

                    true
                }
                None => false,
            }
        },
    );
    // TODO #97 - Return error if timeout or something failed
    // return Err(_);

    let action_result = receiver
        .recv_timeout(RECV_DEFAULT_TIMEOUT_MS)
        .expect("observer dropped before done");

    // action_result should be a json str of the result of the zome function called
    match action_result {
        Ok(json_str) => {
            // write result directly in wasm memory
            runtime_allocate_encode_str(runtime, &json_str)
        }
        Err(_) => Ok(Some(RuntimeValue::I32(
            HcApiReturnCode::ErrorActionResult as i32,
        ))),
    }
}

/// Reduce Call Action
///   1. Checks for correctness of ZomeFnCall inside the Action
///   2. Checks for permission to access Capability
///   3. Execute the exposed Zome function in a separate thread
/// Send the result in a ReturnZomeFunctionResult Action on success or failure like ExecuteZomeFunction
pub(crate) fn reduce_call(
    context: Arc<Context>,
    state: &mut NucleusState,
    action_wrapper: &ActionWrapper,
    action_channel: &Sender<ActionWrapper>,
    observer_channel: &Sender<Observer>,
) {
    // 1.Checks for correctness of ZomeFnCall
    let fn_call = match action_wrapper.action().clone() {
        Action::Call(call) => call,
        _ => unreachable!(),
    };
    // Get Capability
    let maybe_cap = state.get_capability(fn_call.clone());
    if let Err(fn_res) = maybe_cap {
        // Notify failure
        state
            .zome_calls
            .insert(fn_call.clone(), Some(fn_res.result()));
        return;
    }
    let cap = maybe_cap.unwrap();

    // 2. Checks for permission to access Capability
    // TODO #301 - Do real Capability token check
    let can_call = match cap.cap_type.membrane {
        Membrane::Public => true,
        Membrane::Zome => {
            // TODO #301 - check if caller zome_name is same as called zome_name
            false
        }
        Membrane::Agent => {
            // TODO #301 - check if caller has Agent Capability
            false
        }
        Membrane::ApiKey => {
            // TODO #301 - check if caller has ApiKey Capability
            false
        }
    };
    // Notify failure
    if !can_call {
        state.zome_calls.insert(
            fn_call.clone(),
            Some(Err(HolochainError::DoesNotHaveCapabilityToken)),
        );
        return;
    }

    // 3. Execute the exposed Zome function in a separate thread
    state.zome_calls.insert(fn_call.clone(), None);
    launch_zome_fn_call(
        context,
        fn_call,
        action_channel,
        observer_channel,
        &cap.code,
        state.dna.clone().unwrap().name,
    );
}

#[cfg(test)]
pub mod tests {
    extern crate test_utils;
    extern crate wabt;

    use super::*;
    use context::Context;
    use holochain_agent::Agent;
    use holochain_dna::Dna;
    use instance::tests::{test_instance, TestLogger};
    use nucleus::ribosome::{
        api::{
            tests::{
                test_capability, test_function_name, test_parameters, test_zome_api_function_wasm,
                test_zome_name,
            },
            ZomeApiFunction,
        },
        Defn,
    };
    use persister::SimplePersister;
    use serde_json;
    use std::sync::{mpsc::RecvTimeoutError, Arc, Mutex};
    use test_utils::{create_test_cap, create_test_dna_with_cap};

    /// dummy commit args from standard test entry
    pub fn test_bad_args_bytes() -> Vec<u8> {
        let args = ZomeCallArgs {
            zome_name: "zome_name".to_string(),
            cap_name: "cap_name".to_string(),
            fn_name: "fn_name".to_string(),
            fn_args: "fn_args".to_string(),
        };
        serde_json::to_string(&args)
            .expect("args should serialize")
            .into_bytes()
    }

    pub fn test_args_bytes() -> Vec<u8> {
        let args = ZomeCallArgs {
            zome_name: test_zome_name(),
            cap_name: test_capability(),
            fn_name: test_function_name(),
            fn_args: test_parameters(),
        };
        serde_json::to_string(&args)
            .expect("args should serialize")
            .into_bytes()
    }

    fn create_context() -> Arc<Context> {
        Arc::new(Context {
            agent: Agent::from_string("alex".to_string()),
            logger: Arc::new(Mutex::new(TestLogger { log: Vec::new() })),
            persister: Arc::new(Mutex::new(SimplePersister::new())),
        })
    }

    fn test_reduce_call(
        dna: Dna,
        expected: Result<Result<String, HolochainError>, RecvTimeoutError>,
    ) {
        let context = create_context();

        let zome_call = ZomeFnCall::new("test_zome", "test_cap", "test", "{}");
        let zome_call_action = ActionWrapper::new(Action::Call(zome_call.clone()));

        // Set up instance and process the action
        // let instance = Instance::new();
        let instance = test_instance(dna);
        let (sender, receiver) = channel();
        let closure = move |state: &::state::State| {
            // Observer waits for a ribosome_call_result
            let opt_res = state.nucleus().zome_call_result(&zome_call);
            match opt_res {
                Some(res) => {
                    // @TODO never panic in wasm
                    // @see https://github.com/holochain/holochain-rust/issues/159
                    sender
                        .send(res)
                        // the channel stays connected until the first message has been sent
                        // if this fails that means that it was called after having returned done=true
                        .expect("observer called after done");

                    true
                }
                None => false,
            }
        };

        let observer = Observer {
            sensor: Box::new(closure),
        };

        let mut state_observers: Vec<Observer> = Vec::new();
        state_observers.push(observer);
        let (_, rx_observer) = channel::<Observer>();
        instance.process_action(zome_call_action, state_observers, &rx_observer, &context);

        let action_result = receiver.recv_timeout(RECV_DEFAULT_TIMEOUT_MS);

        assert_eq!(expected, action_result);
    }

    #[test]
    fn test_call_no_token() {
        let dna = test_utils::create_test_dna_with_wat("test_zome", "test_cap", None);
        let expected = Ok(Err(HolochainError::DoesNotHaveCapabilityToken));
        test_reduce_call(dna, expected);
    }

    #[test]
    fn test_call_no_zome() {
        let dna = test_utils::create_test_dna_with_wat("bad_zome", "test_cap", None);
        let expected = Ok(Err(HolochainError::ZomeNotFound(
            r#"Zome 'test_zome' not found"#.to_string(),
        )));
        test_reduce_call(dna, expected);
    }

    #[test]
    fn test_call_ok() {
        let wasm = test_zome_api_function_wasm(ZomeApiFunction::Call.as_str());
        let cap = create_test_cap(Membrane::Public, &wasm);
        let dna = create_test_dna_with_cap(&test_zome_name(), "test_cap", &cap);

        // Expecting timeout since there is no function in wasm to call
        let expected = Err(RecvTimeoutError::Disconnected);
        test_reduce_call(dna, expected);
    }
}