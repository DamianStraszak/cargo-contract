// Copyright 2018-2023 Parity Technologies (UK) Ltd.
// This file is part of cargo-contract.
//
// cargo-contract is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// cargo-contract is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with cargo-contract.  If not, see <http://www.gnu.org/licenses/>.

use super::{
    create_signer,
    display_contract_exec_result,
    display_contract_exec_result_debug,
    display_dry_run_result_warning,
    print_dry_running_status,
    print_gas_required_success,
    prompt_confirm_tx,
    CLIExtrinsicOpts,
    MAX_KEY_COL_WIDTH,
};
use crate::{
    anyhow,
    ErrorVariant,
    InstantiateExec,
    Weight,
};
use anyhow::Result;
use contract_build::{
    name_value_println,
    util::{
        decode_hex,
        DEFAULT_KEY_COL_WIDTH,
    },
    Verbosity,
};
use contract_extrinsics::{
    BalanceVariant,
    Code,
    DisplayEvents,
    ExtrinsicOptsBuilder,
    InstantiateCommandBuilder,
    InstantiateDryRunResult,
    InstantiateExecResult,
    TokenMetadata,
};
use ink_env::{
    DefaultEnvironment,
    Environment,
};
use sp_core::Bytes;
use std::fmt::Debug;
use subxt::PolkadotConfig as DefaultConfig;
use subxt_signer::sr25519::Keypair;

#[derive(Debug, clap::Args)]
pub struct InstantiateCommand {
    /// The name of the contract constructor to call
    #[clap(name = "constructor", long, default_value = "new")]
    constructor: String,
    /// The constructor arguments, encoded as strings
    #[clap(long, num_args = 0..)]
    args: Vec<String>,
    #[clap(flatten)]
    extrinsic_cli_opts: CLIExtrinsicOpts,
    /// Transfers an initial balance to the instantiated contract
    #[clap(name = "value", long, default_value = "0")]
    value: BalanceVariant<<DefaultEnvironment as Environment>::Balance>,
    /// Maximum amount of gas to be used for this command.
    /// If not specified will perform a dry-run to estimate the gas consumed for the
    /// instantiation.
    #[clap(name = "gas", long)]
    gas_limit: Option<u64>,
    /// Maximum proof size for this instantiation.
    /// If not specified will perform a dry-run to estimate the proof size required.
    #[clap(long)]
    proof_size: Option<u64>,
    /// A salt used in the address derivation of the new contract. Use to create multiple
    /// instances of the same contract code from the same account.
    #[clap(long, value_parser = parse_hex_bytes)]
    salt: Option<Bytes>,
    /// Export the instantiate output in JSON format.
    #[clap(long, conflicts_with = "verbose")]
    output_json: bool,
}

/// Parse hex encoded bytes.
fn parse_hex_bytes(input: &str) -> Result<Bytes> {
    let bytes = decode_hex(input)?;
    Ok(bytes.into())
}

impl InstantiateCommand {
    /// Returns whether to export the call output in JSON format.
    pub fn output_json(&self) -> bool {
        self.output_json
    }

    pub async fn handle(&self) -> Result<(), ErrorVariant> {
        let token_metadata =
            TokenMetadata::query::<DefaultConfig>(&self.extrinsic_cli_opts.url).await?;

        let signer = create_signer(&self.extrinsic_cli_opts.suri)?;
        let extrinsic_opts = ExtrinsicOptsBuilder::new(signer)
            .file(self.extrinsic_cli_opts.file.clone())
            .manifest_path(self.extrinsic_cli_opts.manifest_path.clone())
            .url(self.extrinsic_cli_opts.url.clone())
            .storage_deposit_limit(
                self.extrinsic_cli_opts
                    .storage_deposit_limit
                    .clone()
                    .map(|bv| bv.denominate_balance(&token_metadata))
                    .transpose()?,
            )
            .done();
        let instantiate_exec: InstantiateExec<
            DefaultConfig,
            DefaultEnvironment,
            Keypair,
        > = InstantiateCommandBuilder::new(extrinsic_opts)
            .constructor(self.constructor.clone())
            .args(self.args.clone())
            .value(self.value.denominate_balance(&token_metadata)?)
            .gas_limit(self.gas_limit)
            .proof_size(self.proof_size)
            .salt(self.salt.clone())
            .done()
            .await?;

        if !self.extrinsic_cli_opts.execute {
            let result = instantiate_exec.instantiate_dry_run().await?;
            match instantiate_exec.decode_instantiate_dry_run(&result).await {
                Ok(dry_run_result) => {
                    if self.output_json() {
                        println!("{}", dry_run_result.to_json()?);
                    } else {
                        print_instantiate_dry_run_result(&dry_run_result);
                        display_contract_exec_result_debug::<_, DEFAULT_KEY_COL_WIDTH>(
                            &result,
                        )?;
                        display_dry_run_result_warning("instantiate");
                    }
                    Ok(())
                }
                Err(object) => {
                    if self.output_json() {
                        return Err(object)
                    } else {
                        name_value_println!("Result", object, MAX_KEY_COL_WIDTH);
                        display_contract_exec_result::<_, MAX_KEY_COL_WIDTH>(&result)?;
                    }
                    Err(object)
                }
            }
        } else {
            tracing::debug!("instantiate data {:?}", instantiate_exec.args().data());
            let gas_limit = pre_submit_dry_run_gas_estimate_instantiate(
                &instantiate_exec,
                self.output_json(),
                self.extrinsic_cli_opts.skip_dry_run,
            )
            .await?;
            if !self.extrinsic_cli_opts.skip_confirm {
                prompt_confirm_tx(|| {
                    print_default_instantiate_preview(&instantiate_exec, gas_limit);
                    if let Code::Existing(code_hash) =
                        instantiate_exec.args().code().clone()
                    {
                        name_value_println!(
                            "Code hash",
                            format!("{code_hash:?}"),
                            DEFAULT_KEY_COL_WIDTH
                        );
                    }
                })?;
            }
            let instantiate_result =
                instantiate_exec.instantiate(Some(gas_limit)).await?;
            display_result(
                &instantiate_exec,
                instantiate_result,
                &token_metadata,
                self.output_json(),
                self.extrinsic_cli_opts.verbosity().unwrap(),
            )
            .await?;
            Ok(())
        }
    }
}

/// A helper function to estimate the gas required for a contract instantiation.
async fn pre_submit_dry_run_gas_estimate_instantiate(
    instantiate_exec: &InstantiateExec<DefaultConfig, DefaultEnvironment, Keypair>,
    output_json: bool,
    skip_dry_run: bool,
) -> Result<Weight> {
    if skip_dry_run {
        return match (instantiate_exec.args().gas_limit(), instantiate_exec.args().proof_size()) {
                (Some(ref_time), Some(proof_size)) => Ok(Weight::from_parts(ref_time, proof_size)),
                _ => {
                    Err(anyhow!(
                        "Weight args `--gas` and `--proof-size` required if `--skip-dry-run` specified"
                    ))
                }
            };
    }
    if !output_json {
        print_dry_running_status(instantiate_exec.args().constructor());
    }
    let instantiate_result = instantiate_exec.instantiate_dry_run().await?;
    match instantiate_result.result {
        Ok(_) => {
            if !output_json {
                print_gas_required_success(instantiate_result.gas_required);
            }
            // use user specified values where provided, otherwise use the estimates
            let ref_time = instantiate_exec
                .args()
                .gas_limit()
                .unwrap_or_else(|| instantiate_result.gas_required.ref_time());
            let proof_size = instantiate_exec
                .args()
                .proof_size()
                .unwrap_or_else(|| instantiate_result.gas_required.proof_size());
            Ok(Weight::from_parts(ref_time, proof_size))
        }
        Err(ref err) => {
            let object = ErrorVariant::from_dispatch_error(
                err,
                &instantiate_exec.client().metadata(),
            )?;
            if output_json {
                Err(anyhow!("{}", serde_json::to_string_pretty(&object)?))
            } else {
                name_value_println!("Result", object, MAX_KEY_COL_WIDTH);
                display_contract_exec_result::<_, MAX_KEY_COL_WIDTH>(
                    &instantiate_result,
                )?;

                Err(anyhow!("Pre-submission dry-run failed. Use --skip-dry-run to skip this step."))
            }
        }
    }
}

/// Displays the results of contract instantiation, including contract address,
/// events, and optional code hash.
pub async fn display_result(
    instantiate_exec: &InstantiateExec<DefaultConfig, DefaultEnvironment, Keypair>,
    instantiate_exec_result: InstantiateExecResult<DefaultConfig>,
    token_metadata: &TokenMetadata,
    output_json: bool,
    verbosity: Verbosity,
) -> Result<(), ErrorVariant> {
    let events = DisplayEvents::from_events::<DefaultConfig, DefaultEnvironment>(
        &instantiate_exec_result.events,
        Some(instantiate_exec.transcoder()),
        &instantiate_exec.client().metadata(),
    )?;
    let contract_address = instantiate_exec_result.contract_address.to_string();
    if output_json {
        let display_instantiate_result = InstantiateResult {
            code_hash: instantiate_exec_result
                .code_hash
                .map(|ch| format!("{ch:?}")),
            contract: Some(contract_address),
            events,
        };
        println!("{}", display_instantiate_result.to_json()?)
    } else {
        println!(
            "{}",
            events.display_events::<DefaultEnvironment>(verbosity, token_metadata)?
        );
        if let Some(code_hash) = instantiate_exec_result.code_hash {
            name_value_println!("Code hash", format!("{code_hash:?}"));
        }
        name_value_println!("Contract", contract_address);
    };
    Ok(())
}

pub fn print_default_instantiate_preview(
    instantiate_exec: &InstantiateExec<DefaultConfig, DefaultEnvironment, Keypair>,
    gas_limit: Weight,
) {
    name_value_println!(
        "Constructor",
        instantiate_exec.args().constructor(),
        DEFAULT_KEY_COL_WIDTH
    );
    name_value_println!(
        "Args",
        instantiate_exec.args().raw_args().join(" "),
        DEFAULT_KEY_COL_WIDTH
    );
    name_value_println!("Gas limit", gas_limit.to_string(), DEFAULT_KEY_COL_WIDTH);
}

/// Result of a successful contract instantiation for displaying.
#[derive(serde::Serialize)]
pub struct InstantiateResult {
    /// Instantiated contract hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract: Option<String>,
    /// Instantiated code hash
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_hash: Option<String>,
    /// The events emitted from the instantiate extrinsic invocation.
    pub events: DisplayEvents,
}

impl InstantiateResult {
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

pub fn print_instantiate_dry_run_result(
    result: &InstantiateDryRunResult<<DefaultEnvironment as Environment>::Balance>,
) {
    name_value_println!(
        "Result",
        format!("{}", result.result),
        DEFAULT_KEY_COL_WIDTH
    );
    name_value_println!(
        "Reverted",
        format!("{:?}", result.reverted),
        DEFAULT_KEY_COL_WIDTH
    );
    name_value_println!("Contract", result.contract, DEFAULT_KEY_COL_WIDTH);
    name_value_println!(
        "Gas consumed",
        result.gas_consumed.to_string(),
        DEFAULT_KEY_COL_WIDTH
    );
}
