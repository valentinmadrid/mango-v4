use anchor_lang::prelude::*;
use anchor_spl::dex;

use crate::state::*;

#[derive(Accounts)]
pub struct CreateSerumOpenOrders<'info> {
    // TODO: do we even need the group?
    pub group: AccountLoader<'info, Group>,

    #[account(
        mut,
        has_one = group,
        has_one = owner,
    )]
    pub account: AccountLoader<'info, MangoAccount>,

    #[account(
        has_one = group,
        has_one = serum_program,
        has_one = serum_market_external,
    )]
    pub serum_market: AccountLoader<'info, SerumMarket>,

    // TODO: limit?
    pub serum_program: UncheckedAccount<'info>,
    pub serum_market_external: UncheckedAccount<'info>,

    // initialized by this instruction via cpi to serum
    #[account(
        init,
        seeds = [account.key().as_ref(), b"serumoo".as_ref(), serum_market.key().as_ref()],
        bump,
        payer = payer,
        owner = serum_program.key(),
        // 12 is the padding serum uses for accounts ("serum" prefix, "padding" postfix)
        space = 12 + std::mem::size_of::<serum_dex::state::OpenOrders>(),
    )]
    pub open_orders: UncheckedAccount<'info>,

    pub owner: Signer<'info>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn create_serum_open_orders(ctx: Context<CreateSerumOpenOrders>) -> Result<()> {
    let serum_market = ctx.accounts.serum_market.load()?;
    let context = CpiContext::new(
        ctx.accounts.serum_program.to_account_info(),
        dex::InitOpenOrders {
            open_orders: ctx.accounts.open_orders.to_account_info(),
            // TODO: Should the authority be the account or the market account or the group?
            authority: ctx.accounts.serum_market.to_account_info(),
            market: ctx.accounts.serum_market_external.to_account_info(),
            rent: ctx.accounts.rent.to_account_info(),
        },
    );
    let seeds = serum_market_seeds!(serum_market);
    // TODO: Anchor's code _forces_ anchor_spl::dex::id() as a program id.
    //       Are we ok with that? that would mean storing serum_program is not
    //       necessary.
    dex::init_open_orders(context.with_signer(&[seeds]))?;

    let mut account = ctx.accounts.account.load_mut()?;
    let oos = account
        .serum_open_orders_map
        .create(serum_market.market_index)?;
    oos.open_orders = ctx.accounts.open_orders.key();

    Ok(())
}