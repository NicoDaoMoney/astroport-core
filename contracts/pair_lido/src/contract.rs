// SPDX-License-Identifier: GPL-3.0-only
// Copyright Astroport
// Copyright Lido

use crate::error::ContractError;
use crate::state::{Config, SwapRequestInfo, CONFIG, SWAP_REQUEST};
use cosmwasm_bignumber::Decimal256;
use std::str::FromStr;

use cosmwasm_std::{
    entry_point, from_binary, to_binary, Addr, Binary, CosmosMsg, Decimal, Deps, DepsMut, Env,
    MessageInfo, Reply, Response, StdError, StdResult, SubMsg, Uint128, WasmMsg,
};

use crate::queries::query_total_tokens_issued;
use crate::simulation::{
    convert_bluna_to_stluna, convert_stluna_to_bluna, get_required_bluna, get_required_stluna,
};
use astroport::asset::{addr_validate_to_lower, Asset, AssetInfo, PairInfo};
use astroport::factory::PairType;
use astroport::pair::InstantiateMsg;
use astroport::pair::{
    CumulativePricesResponse, Cw20HookMsg, ExecuteMsg, MigrateMsg, PoolResponse, QueryMsg,
    ReverseSimulationResponse, SimulationResponse, TWAP_PRECISION,
};
use astroport::pair_lido::{
    ConfigResponse, LidoPoolParams, DEFAULT_SLIPPAGE, MAX_ALLOWED_SLIPPAGE,
};
use astroport::querier::query_fee_info;
use basset::hub::Cw20HookMsg as HubCw20HookMsg;
use cw2::set_contract_version;
use cw20::Cw20ReceiveMsg;
use std::vec;

const SWAP_REPLY_ID: u64 = 1;

/// Contract name that is used for migration.
const CONTRACT_NAME: &str = "astroport-pair-lido";
/// Contract version that is used for migration.
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// ## Description
/// Creates a new contract with the specified parameters in the [`InstantiateMsg`].
/// Returns the [`Response`] with the specified attributes if the operation was successful, or a [`ContractError`] if the contract was not created
/// ## Params
/// * **deps** is the object of type [`DepsMut`].
///
/// * **env** is the object of type [`Env`].
///
/// * **_info** is the object of type [`MessageInfo`].
/// * **msg** is a message of type [`InstantiateMsg`] which contains the basic settings for creating a contract
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    if msg.asset_infos[0].is_native_token() || msg.asset_infos[1].is_native_token() {
        return Err(ContractError::NonSupported {});
    }

    msg.asset_infos[0].check(deps.api)?;
    msg.asset_infos[1].check(deps.api)?;

    if msg.asset_infos[0] == msg.asset_infos[1] {
        return Err(ContractError::DoublingAssets {});
    }

    if msg.init_params.is_none() {
        return Err(ContractError::InitParamsNotFound {});
    }

    let params: LidoPoolParams = from_binary(&msg.init_params.unwrap())?;

    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    let config = Config {
        pair_info: PairInfo {
            asset_infos: msg.asset_infos.clone(),
            contract_addr: env.contract.address,
            liquidity_token: Addr::unchecked(""),
            pair_type: PairType::Custom("Lido-XYK".to_string()),
        },
        hub_addr: addr_validate_to_lower(deps.api, params.hub_address.as_str())?,
        stluna_addr: addr_validate_to_lower(deps.api, params.stluna_addr.as_str())?,
        block_time_last: 0,
        price0_cumulative_last: Uint128::zero(),
        price1_cumulative_last: Uint128::zero(),
        factory_addr: addr_validate_to_lower(deps.api, msg.factory_addr.as_str())?,
        bluna_addr: addr_validate_to_lower(deps.api, params.bluna_addr.as_str())?,
    };

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new())
}

/// ## Description
/// Available the execute messages of the contract.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **env** is the object of type [`Env`].
///
/// * **info** is the object of type [`MessageInfo`].
///
/// * **msg** is the object of type [`ExecuteMsg`].
///
/// ## Queries
/// * **ExecuteMsg::UpdateConfig { params: Binary }** Not supported.
///
/// * **ExecuteMsg::Receive(msg)** Receives a message of type [`Cw20ReceiveMsg`] and processes
/// it depending on the received template.
///
/// * **ExecuteMsg::ProvideLiquidity {
///             assets,
///             slippage_tolerance,
///             auto_stake,
///             receiver,
///         }** Provides liquidity with the specified input parameters.
///
/// * **ExecuteMsg::Swap {
///             offer_asset,
///             belief_price,
///             max_spread,
///             to,
///         }** Performs an swap operation with the specified parameters.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::UpdateConfig { .. } => Err(ContractError::NonSupported {}),
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::ProvideLiquidity { .. } => Err(ContractError::NonSupported {}),
        ExecuteMsg::Swap { .. } => Err(ContractError::NonSupported {}),
    }
}

/// ## Description
/// Receives a message of type [`Cw20ReceiveMsg`] and processes it depending on the received template.
/// If the template is not found in the received message, then an [`ContractError`] is returned,
/// otherwise returns the [`Response`] with the specified attributes if the operation was successful
/// ## Params
/// * **deps** is the object of type [`DepsMut`].
///
/// * **env** is the object of type [`Env`].
///
/// * **info** is the object of type [`MessageInfo`].
///
/// * **cw20_msg** is the object of type [`Cw20ReceiveMsg`].
pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> Result<Response, ContractError> {
    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Swap {
            belief_price,
            max_spread,
            to,
        }) => {
            // Only an asset (token) contract can execute this message
            let mut authorized: bool = false;
            let config: Config = CONFIG.load(deps.storage)?;

            for pool in config.clone().pair_info.asset_infos {
                if let AssetInfo::Token { contract_addr, .. } = &pool {
                    if contract_addr == &info.sender {
                        authorized = true;
                    }
                }
            }

            if !authorized {
                return Err(ContractError::Unauthorized {});
            }

            let to_addr = if let Some(to_addr) = to {
                Some(addr_validate_to_lower(deps.api, to_addr.as_str())?)
            } else {
                None
            };

            swap(
                deps,
                env,
                config,
                Addr::unchecked(cw20_msg.sender),
                Asset {
                    info: AssetInfo::Token {
                        contract_addr: info.sender,
                    },
                    amount: cw20_msg.amount,
                },
                belief_price,
                max_spread,
                to_addr,
            )
        }
        Ok(Cw20HookMsg::WithdrawLiquidity {}) => Err(ContractError::NonSupported {}),
        Err(err) => Err(err.into()),
    }
}

/// ## Description
/// Performs an swap operation with the specified parameters. CONTRACT - a user must do token approval.
/// Returns an [`ContractError`] on failure, otherwise returns the [`Response`] with the specified attributes if the operation was successful.
/// ## Params
/// * **deps** is the object of type [`DepsMut`].
///
/// * **env** is the object of type [`Env`].
///
/// * **info** is the object of type [`MessageInfo`].
///
/// * **sender** is the object of type [`Addr`]. Sets the default recipient of the swap operation.
///
/// * **offer_asset** is the object of type [`Asset`]. Proposed asset for swapping.
///
/// * **belief_price** is the object of type [`Option<Decimal>`]. Used to calculate the maximum spread.
///
/// * **max_spread** is the object of type [`Option<Decimal>`]. Sets the maximum spread of the swap operation.
///
/// * **to** is the object of type [`Option<Addr>`]. Sets the recipient of the swap operation.
#[allow(clippy::too_many_arguments)]
pub fn swap(
    deps: DepsMut,
    env: Env,
    config: Config,
    sender: Addr,
    offer_asset: Asset,
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    to: Option<Addr>,
) -> Result<Response, ContractError> {
    let pools: Vec<Asset> = config
        .pair_info
        .query_pools(&deps.querier, env.contract.address)?
        .to_vec();

    let offer_pool: Asset;
    let ask_pool: Asset;

    if offer_asset.info.equal(&pools[0].info) {
        offer_pool = pools[0].clone();
        ask_pool = pools[1].clone();
    } else if offer_asset.info.equal(&pools[1].info) {
        offer_pool = pools[1].clone();
        ask_pool = pools[0].clone();
    } else {
        return Err(ContractError::AssetMismatch {});
    }

    let swap_request_info = SwapRequestInfo {
        ask_asset_info: ask_pool.info.clone(),
        offer_asset_info: Asset {
            info: offer_pool.info,
            amount: offer_asset.amount,
        },
        belief_price,
        max_spread,
    };

    // saving recipient of the swap operation and ask token address to the storage
    // to send swapped tokens to the recipient in reply handler
    if let Some(to_addr) = to {
        SWAP_REQUEST.save(deps.storage, &(to_addr, swap_request_info))?;
    } else {
        SWAP_REQUEST.save(deps.storage, &(sender, swap_request_info))?;
    }

    let mut sub_messages: Vec<SubMsg> = vec![];

    if ask_pool.amount > Uint128::zero() {
        // Get fee info from the factory
        let fee_info = query_fee_info(
            &deps.querier,
            config.factory_addr.clone(),
            config.pair_info.pair_type.clone(),
        )?;

        // if someone deposited into the pair contract instance
        // the balance will be transferred to the maker address
        if let Some(fee_address) = fee_info.fee_address {
            // send funds to maker address
            sub_messages.push(SubMsg::new(
                ask_pool.into_msg(&deps.querier, fee_address).unwrap(),
            ))
        }
    }

    match offer_asset.info {
        AssetInfo::Token { contract_addr } => sub_messages.push(SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: contract_addr.to_string(),
                msg: to_binary(&cw20::Cw20ExecuteMsg::Send {
                    contract: config.hub_addr.to_string(),
                    amount: offer_asset.amount,
                    msg: to_binary(&HubCw20HookMsg::Convert {})?,
                })?,
                funds: vec![],
            }),
            SWAP_REPLY_ID,
        )),

        AssetInfo::NativeToken { .. } => {
            return Err(ContractError::NonSupported {});
        }
    }

    Ok(Response::new().add_submessages(sub_messages))
}

/// # Description
/// The entry point to the contract for processing the reply from the submessage
/// # Params
/// * **deps** is the object of type [`DepsMut`].
///
/// * **_env** is the object of type [`Env`].
///
/// * **msg** is the object of type [`Reply`].
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, env: Env, _msg: Reply) -> Result<Response, ContractError> {
    let swap_request = SWAP_REQUEST.load(deps.storage)?;

    let return_amount = swap_request
        .1
        .ask_asset_info
        .query_pool(&deps.querier, env.contract.address.clone())?;

    // Check the max spread limit (if it was specified)
    assert_max_spread(
        swap_request.1.belief_price,
        swap_request.1.max_spread,
        swap_request.1.offer_asset_info.amount,
        return_amount,
    )?;

    let mut config = CONFIG.load(deps.storage)?;

    if let Some((price0_cumulative_new, price1_cumulative_new, block_time)) =
        accumulate_prices(deps.as_ref(), env, &config)?
    {
        config.price0_cumulative_last = price0_cumulative_new;
        config.price1_cumulative_last = price1_cumulative_new;
        config.block_time_last = block_time;
        CONFIG.save(deps.storage, &config)?;
    }

    let return_asset = Asset {
        info: swap_request.1.ask_asset_info,
        amount: return_amount,
    };

    Ok(Response::new().add_message(return_asset.into_msg(&deps.querier, swap_request.0)?))
}

/// ## Description
/// Available the query messages of the contract.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **_env** is the object of type [`Env`].
///
/// * **msg** is the object of type [`QueryMsg`].
///
/// ## Queries
/// * **QueryMsg::Pair {}** Returns information about a pair in an object of type [`PairInfo`].
///
/// * **QueryMsg::Pool {}** Returns information about a pool in an object of type [`PoolResponse`].
///
/// * **QueryMsg::Share { amount }** Returns information about the share of the pool in a vector
/// that contains objects of type [`Asset`].
///
/// * **QueryMsg::Simulation { offer_asset }** Returns information about the simulation of the
/// swap in a [`SimulationResponse`] object.
///
/// * **QueryMsg::ReverseSimulation { ask_asset }** Returns information about the reverse simulation
/// in a [`ReverseSimulationResponse`] object.
///
/// * **QueryMsg::CumulativePrices {}** Returns information about the cumulative prices in a
/// [`CumulativePricesResponse`] object.
///
/// * **QueryMsg::Config {}** Returns information about the controls settings in a
/// [`ConfigResponse`] object.
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Pair {} => to_binary(&query_pair_info(deps)?),
        QueryMsg::Pool {} => to_binary(&query_pool(deps)?),
        QueryMsg::Share { amount } => to_binary(&query_share(deps, amount)?),
        QueryMsg::Simulation { offer_asset } => to_binary(&query_simulation(deps, offer_asset)?),
        QueryMsg::ReverseSimulation { ask_asset } => {
            to_binary(&query_reverse_simulation(deps, ask_asset)?)
        }
        QueryMsg::CumulativePrices {} => to_binary(&query_cumulative_prices(deps, env)?),
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
    }
}

/// ## Description
/// Returns information about a pair in an object of type [`PairInfo`].
/// ## Params
/// * **deps** is the object of type [`Deps`].
pub fn query_pair_info(deps: Deps) -> StdResult<PairInfo> {
    let config: Config = CONFIG.load(deps.storage)?;
    Ok(config.pair_info)
}

/// ## Description
/// Returns information about a pool in an object of type [`PoolResponse`].
/// ## Params
/// * **deps** is the object of type [`Deps`].
pub fn query_pool(deps: Deps) -> StdResult<PoolResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let (assets, total_share) = pool_info(deps, config)?;

    let resp = PoolResponse {
        assets,
        total_share,
    };

    Ok(resp)
}

/// ## Description
/// Returns information about the share of the pool in a vector that contains objects of type [`Asset`].
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **amount** is the object of type [`Uint128`]. Sets the amount for which a share in the pool will be requested.
pub fn query_share(_deps: Deps, _amount: Uint128) -> StdResult<Vec<Asset>> {
    Ok(vec![])
}

/// ## Description
/// Returns information about the simulation of the swap in a [`SimulationResponse`] object.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **offer_asset** is the object of type [`Asset`].
pub fn query_simulation(deps: Deps, offer_asset: Asset) -> StdResult<SimulationResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let pools: [AssetInfo; 2] = config.pair_info.asset_infos.clone();

    if !offer_asset.info.equal(&pools[0]) && !offer_asset.info.equal(&pools[1]) {
        return Err(StdError::generic_err(
            "Given offer asset doesn't belong to pairs",
        ));
    }

    if let AssetInfo::Token { contract_addr } = offer_asset.info {
        if contract_addr == config.stluna_addr {
            Ok(SimulationResponse {
                return_amount: convert_stluna_to_bluna(deps, config, offer_asset.amount)?,
                spread_amount: Uint128::zero(),
                commission_amount: Uint128::zero(),
            })
        } else if contract_addr == config.bluna_addr {
            Ok(SimulationResponse {
                return_amount: convert_bluna_to_stluna(deps, config, offer_asset.amount)?,
                spread_amount: Uint128::zero(),
                commission_amount: Uint128::zero(),
            })
        } else {
            Err(StdError::generic_err("invalid offer asset"))
        }
    } else {
        Err(StdError::generic_err("invalid offer asset"))
    }
}

/// ## Description
/// Returns information about the reverse simulation in a [`ReverseSimulationResponse`] object.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **ask_asset** is the object of type [`Asset`].
pub fn query_reverse_simulation(
    deps: Deps,
    ask_asset: Asset,
) -> StdResult<ReverseSimulationResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    let pools: [AssetInfo; 2] = config.pair_info.asset_infos.clone();

    if !ask_asset.info.equal(&pools[0]) && !ask_asset.info.equal(&pools[1]) {
        return Err(StdError::generic_err(
            "Given ask asset doesn't belong to pairs",
        ));
    }

    if let AssetInfo::Token { contract_addr } = ask_asset.info {
        if contract_addr == config.stluna_addr {
            Ok(ReverseSimulationResponse {
                offer_amount: get_required_bluna(deps, config, ask_asset.amount)?,
                spread_amount: Uint128::zero(),
                commission_amount: Uint128::zero(),
            })
        } else if contract_addr == config.bluna_addr {
            Ok(ReverseSimulationResponse {
                offer_amount: get_required_stluna(deps, config, ask_asset.amount)?,
                spread_amount: Uint128::zero(),
                commission_amount: Uint128::zero(),
            })
        } else {
            Err(StdError::generic_err("invalid ask asset"))
        }
    } else {
        Err(StdError::generic_err("invalid ask asset"))
    }
}

/// ## Description
/// Returns information about the cumulative prices in a [`CumulativePricesResponse`] object.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **env** is the object of type [`Env`].
pub fn query_cumulative_prices(deps: Deps, env: Env) -> StdResult<CumulativePricesResponse> {
    let config = CONFIG.load(deps.storage)?;
    let (assets, total_share) = pool_info(deps, config.clone())?;

    let mut price0_cumulative_last = config.price0_cumulative_last;
    let mut price1_cumulative_last = config.price1_cumulative_last;

    if let Some((price0_cumulative_new, price1_cumulative_new, _)) =
        accumulate_prices(deps, env, &config)?
    {
        price0_cumulative_last = price0_cumulative_new;
        price1_cumulative_last = price1_cumulative_new;
    }

    let resp = CumulativePricesResponse {
        assets,
        total_share,
        price0_cumulative_last,
        price1_cumulative_last,
    };

    Ok(resp)
}

/// ## Description
/// Returns information about the controls settings in a [`ConfigResponse`] object.
/// ## Params
/// * **deps** is the object of type [`Deps`].
pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config: Config = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        hub_address: config.hub_addr,
        stluna_address: config.stluna_addr,
        bluna_address: config.bluna_addr,
        block_time_last: config.block_time_last,
    })
}

/// ## Description
/// Returns a [`ContractError`] on failure.
/// If `belief_price` and `max_spread` are both specified, we compute a new spread,
/// otherwise we just use the swap spread to check `max_spread`.
/// ## Params
/// * **belief_price** is an object of type [`Option<Decimal>`]. This is the belief price used in the swap.
///
/// * **max_spread** is an object of type [`Option<Decimal>`]. This is the
/// max spread allowed so that the swap can be executed successfuly.
///
/// * **offer_amount** is an object of type [`Uint128`]. This is the amount of assets to swap.
///
/// * **return_amount** is an object of type [`Uint128`]. This is the amount of assets to receive from the swap.
pub fn assert_max_spread(
    belief_price: Option<Decimal>,
    max_spread: Option<Decimal>,
    offer_amount: Uint128,
    return_amount: Uint128,
) -> Result<(), ContractError> {
    let default_spread = Decimal::from_str(DEFAULT_SLIPPAGE)?;
    let max_allowed_spread = Decimal::from_str(MAX_ALLOWED_SLIPPAGE)?;

    let max_spread = max_spread.unwrap_or(default_spread);
    if max_spread.gt(&max_allowed_spread) {
        return Err(ContractError::AllowedSpreadAssertion {});
    }

    if let Some(belief_price) = belief_price {
        let expected_return =
            offer_amount * Decimal::from(Decimal256::one() / Decimal256::from(belief_price));
        let spread_amount = expected_return.saturating_sub(return_amount);

        if return_amount < expected_return
            && Decimal::from_ratio(spread_amount, expected_return) > max_spread
        {
            return Err(ContractError::MaxSpreadAssertion {});
        }
    }

    Ok(())
}

/// ## Description
/// Used for migration of contract. Returns the default object of type [`Response`].
/// ## Params
/// * **_deps** is the object of type [`DepsMut`].
///
/// * **_env** is the object of type [`Env`].
///
/// * **_msg** is the object of type [`MigrateMsg`].
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    Ok(Response::default())
}

/// ## Description
/// Returns information about the pool.
/// ## Params
/// * **deps** is the object of type [`Deps`].
///
/// * **config** is the object of type [`Config`].
pub fn pool_info(deps: Deps, config: Config) -> StdResult<([Asset; 2], Uint128)> {
    Ok((
        [
            Asset {
                info: AssetInfo::Token {
                    contract_addr: config.stluna_addr.clone(),
                },
                amount: query_total_tokens_issued(deps, config.stluna_addr)?,
            },
            Asset {
                info: AssetInfo::Token {
                    contract_addr: config.bluna_addr.clone(),
                },
                amount: query_total_tokens_issued(deps, config.bluna_addr)?,
            },
        ],
        Uint128::zero(),
    ))
}

/// ## Description
/// Shifts block_time when any price is zero to not fill an accumulator with a new price to that period.
/// ## Params
/// * **env** is the object of type [`Env`].
///
/// * **config** is the object of type [`Config`].
///
/// * **stluna_exchange_rate** is the exchange rate of stLuna token
///
/// * **bluna_exchange_rate** is the exchange rate of bLuna token
pub fn accumulate_prices(
    deps: Deps,
    env: Env,
    config: &Config,
) -> StdResult<Option<(Uint128, Uint128, u64)>> {
    let block_time = env.block.time.seconds();
    if block_time <= config.block_time_last {
        return Ok(None);
    }

    // we have to shift block_time when any price is zero to not fill an accumulator with a new price to that period

    let time_elapsed = Uint128::from(block_time - config.block_time_last);

    let stluna_price = convert_stluna_to_bluna(
        deps,
        config.clone(),
        Uint128::from(10u128.pow(TWAP_PRECISION.into())),
    )?;
    let bluna_price = convert_bluna_to_stluna(
        deps,
        config.clone(),
        Uint128::from(10u128.pow(TWAP_PRECISION.into())),
    )?;

    let pcl0 = config
        .price0_cumulative_last
        .wrapping_add(time_elapsed.checked_mul(stluna_price)?);
    let pcl1 = config
        .price1_cumulative_last
        .wrapping_add(time_elapsed.checked_mul(bluna_price)?);
    Ok(Some((pcl0, pcl1, block_time)))
}
