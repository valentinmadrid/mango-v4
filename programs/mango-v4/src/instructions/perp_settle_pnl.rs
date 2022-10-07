use anchor_lang::prelude::*;
use checked_math as cm;
use fixed::types::I80F48;

use crate::accounts_zerocopy::*;
use crate::error::*;
use crate::state::new_health_cache;
use crate::state::Bank;
use crate::state::HealthType;
use crate::state::MangoAccount;
use crate::state::ScanningAccountRetriever;
use crate::state::{AccountLoaderDynamic, Group, PerpMarket};

#[derive(Accounts)]
pub struct PerpSettlePnl<'info> {
    pub group: AccountLoader<'info, Group>,

    #[account(
        mut,
        has_one = group,
        // settler_owner is checked at #1
    )]
    pub settler: AccountLoaderDynamic<'info, MangoAccount>,
    pub settler_owner: Signer<'info>,

    #[account(has_one = group, has_one = oracle)]
    pub perp_market: AccountLoader<'info, PerpMarket>,

    // This account MUST be profitable
    #[account(mut, has_one = group)]
    pub account_a: AccountLoaderDynamic<'info, MangoAccount>,
    // This account MUST have a loss
    #[account(mut, has_one = group)]
    pub account_b: AccountLoaderDynamic<'info, MangoAccount>,

    /// CHECK: Oracle can have different account types, constrained by address in perp_market
    pub oracle: UncheckedAccount<'info>,

    #[account(mut, has_one = group)]
    pub settle_bank: AccountLoader<'info, Bank>,

    /// CHECK: Oracle can have different account types
    #[account(address = settle_bank.load()?.oracle)]
    pub settle_oracle: UncheckedAccount<'info>,
}

pub fn perp_settle_pnl(ctx: Context<PerpSettlePnl>) -> Result<()> {
    // Cannot settle with yourself
    require!(
        ctx.accounts.account_a.key() != ctx.accounts.account_b.key(),
        MangoError::CannotSettleWithSelf
    );

    let (perp_market_index, settle_token_index) = {
        let perp_market = ctx.accounts.perp_market.load()?;
        (
            perp_market.perp_market_index,
            perp_market.settle_token_index,
        )
    };

    let mut account_a = ctx.accounts.account_a.load_mut()?;
    let mut account_b = ctx.accounts.account_b.load_mut()?;

    // check positions exist, for nicer error messages
    {
        account_a.perp_position(perp_market_index)?;
        account_a.token_position(settle_token_index)?;
        account_b.perp_position(perp_market_index)?;
        account_b.token_position(settle_token_index)?;
    }

    let a_init_health;
    let a_maint_health;
    let b_settle_health;
    {
        let retriever =
            ScanningAccountRetriever::new(ctx.remaining_accounts, &ctx.accounts.group.key())
                .context("create account retriever")?;
        b_settle_health = new_health_cache(&account_b.borrow(), &retriever)?.perp_settle_health();
        let a_cache = new_health_cache(&account_a.borrow(), &retriever)?;
        a_init_health = a_cache.health(HealthType::Init);
        a_maint_health = a_cache.health(HealthType::Maint);
    };

    // Account B is the one that must have negative pnl. Check how much of that may be actualized
    // given the account's health. In that, we only care about the health of spot assets on the account.
    // Example: With +100 USDC and -2 SOL (-80 USD) and -500 USD PNL the account may still settle
    //   100 - 1.1*80 = 12 USD perp pnl, even though the overall health is already negative.
    //   Further settlement would convert perp-losses into token-losses and isn't allowed.
    require!(b_settle_health >= 0, MangoError::HealthMustBePositive);

    let mut bank = ctx.accounts.settle_bank.load_mut()?;
    let perp_market = ctx.accounts.perp_market.load()?;

    // Verify that the bank is the quote currency bank
    require!(
        bank.token_index == settle_token_index,
        MangoError::InvalidBank
    );

    // Get oracle price for market. Price is validated inside
    let oracle_price =
        perp_market.oracle_price(&AccountInfoRef::borrow(ctx.accounts.oracle.as_ref())?)?;

    // Fetch perp positions for accounts
    let a_perp_position = account_a.perp_position_mut(perp_market_index)?;
    let b_perp_position = account_b.perp_position_mut(perp_market_index)?;

    // Settle funding before settling any PnL
    a_perp_position.settle_funding(&perp_market);
    b_perp_position.settle_funding(&perp_market);

    // Calculate PnL for each account
    let a_base_native = a_perp_position.base_position_native(&perp_market);
    let b_base_native = b_perp_position.base_position_native(&perp_market);
    let a_pnl: I80F48 = cm!(a_perp_position.quote_position_native() + a_base_native * oracle_price);
    let b_pnl: I80F48 = cm!(b_perp_position.quote_position_native() + b_base_native * oracle_price);

    // Account A must be profitable, and B must be unprofitable
    // PnL must be opposite signs for there to be a settlement
    require!(a_pnl.is_positive(), MangoError::ProfitabilityMismatch);
    require!(b_pnl.is_negative(), MangoError::ProfitabilityMismatch);

    // Settle for the maximum possible capped to b's settle health
    let settlement = a_pnl.abs().min(b_pnl.abs()).min(b_settle_health);
    a_perp_position.change_quote_position(-settlement);
    b_perp_position.change_quote_position(settlement);

    // A percentage fee is paid to the settler when account_a's health is low.
    // That's because the settlement could avoid it getting liquidated.
    let low_health_fee = if a_init_health < 0 {
        let fee_fraction = I80F48::from_num(perp_market.settle_fee_fraction_low_health);
        if a_maint_health < 0 {
            cm!(settlement * fee_fraction)
        } else {
            cm!(settlement * fee_fraction * (-a_init_health / (a_maint_health - a_init_health)))
        }
    } else {
        I80F48::ZERO
    };

    // The settler receives a flat fee
    let flat_fee = I80F48::from_num(perp_market.settle_fee_flat);

    // Fees only apply when the settlement is large enough
    let fee = if settlement >= perp_market.settle_fee_amount_threshold {
        cm!(low_health_fee + flat_fee).min(settlement)
    } else {
        I80F48::ZERO
    };

    // Update the account's net_settled with the new PnL.
    // Applying the fee here means that it decreases the displayed perp pnl.
    let settlement_i64 = settlement.checked_to_num::<i64>().unwrap();
    let fee_i64 = fee.checked_to_num::<i64>().unwrap();
    cm!(account_a.fixed.net_settled += settlement_i64 - fee_i64);
    cm!(account_b.fixed.net_settled -= settlement_i64);

    // Transfer token balances
    // The fee is paid by the account with positive unsettled pnl
    let a_token_position = account_a.token_position_mut(settle_token_index)?.0;
    let b_token_position = account_b.token_position_mut(settle_token_index)?.0;
    bank.deposit(a_token_position, cm!(settlement - fee))?;
    bank.withdraw_with_fee(b_token_position, settlement)?;

    // settler might be the same as account a or b
    drop(account_a);
    drop(account_b);

    let mut settler = ctx.accounts.settler.load_mut()?;
    // account constraint #1
    require!(
        settler
            .fixed
            .is_owner_or_delegate(ctx.accounts.settler_owner.key()),
        MangoError::SomeError
    );

    let (settler_token_position, settler_token_raw_index, _) =
        settler.ensure_token_position(settle_token_index)?;
    if !bank.deposit(settler_token_position, fee)? {
        settler.deactivate_token_position(settler_token_raw_index);
    }

    msg!("settled pnl = {}, fee = {}", settlement, fee);
    Ok(())
}
