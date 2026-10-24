# Trade8:Pnl - Business Requirements & Functionality

> Audience: product owners, business analysts, and anyone evaluating what
> Trade8:Pnl **does for the business**. For architecture read `README.md`;
> for wire-level detail read `DESIGN.md`.

## 1. Business Purpose

- Tell every trader **what they made, right now**: per-symbol and total day
  P&L (realized, unrealized, fees, net) that ticks with every fill and mark.
- Give managers the **office and desk-wide picture** with the same numbers
  the traders see - one source of truth, no shadow spreadsheets.
- Provide supervisory systems (Risk) a **trustworthy day summary** to drive
  loss ladders automatically.
- Guarantee **reconstructable money**: today's numbers replay from fills +
  opening balances; overnight positions carry forward with explicit
  reconciliation and visible mismatches instead of silent cost drift.

## 2. Business Scope (What the Service Provides)

| Capability | Description |
|---|---|
| Live positions | Per (trader, symbol) signed position with average cost and marks |
| Realized P&L | Computed on reducing/closing/reversing fills at fill price vs cost |
| Unrealized P&L | Marked at the last trade print, refreshed on every print |
| Fee accumulation | All fee types reported with fills, converted to the ×10⁴ money scale via currency rates |
| LFT tracking | Max-net high-water and giveback-from-peak, per trader and per symbol |
| Trader day summary | Gross/fees/net/shares/max-net/unrealized/LFT, real-time |
| Office roll-up | Office-day totals folded from member traders (manager visibility) |
| Desk board | All-traders day board (env-manager visibility) |
| Overnight carry | Session close writes opening balances for tomorrow; boot carries them in |
| Overnight verification | Restored parked orders reconcile carried P&L; mismatches surface to ops |
| Session close | Scheduled persist of all positions/resumes/summaries/balances |
| Admin workspace | RFW pages: trader board, overnight reconciliation queue |

### What the service does NOT own (business boundary)

| Concern | Owned by |
|---|---|
| Order entry/state, parking | Trade8:Desk |
| Fills and fee *calculation* | Trade8:Router |
| Market data / prints | Trade8:Feed |
| Risk gating and loss-ladder *decisions* | Trade8:Risk (consumes Pnl summaries) |
| Historical bars | Trade8:Bars |
| Identity | Sentin8:Auth + Shield |

## 3. Business Capabilities

| Area | Capability | Business rule |
|---|---|---|
| Positions | Signed position tracking | Buys add, sells subtract; reversal re-bases average cost at the reversing fill |
| Realized | Close/reduce realization | Realized = (fill vs cost) × closed shares, signed by position side; at reversal the closing portion realizes first |
| Unrealized | Live marks | Last print marks every affected trader's book instantly |
| Fees | Multi-currency accumulation | Each reported fee converts at its currency rate; totals are ×10⁴-accurate |
| LFT | Peak-to-now giveback | Max net profit and the drawdown from it, updated on every fill/mark - the trader's "give it back" meter |
| Summaries | Real-time day board | Trader and office summaries recomputed on every fill and mark |
| Overnight | Continuity | Tomorrow opens exactly with today's closes; restored parked orders reconcile to the penny or raise a mismatch |
| Replay | Reconstructability | Any day's P&L recomputes from fills + opening balances byte-identically |

## 4. Business Events (Outbound)

| Event | Fires when |
|---|---|
| `trade8:pnl:position:updated` | A fill changes a position (persist tx) |
| `trade8:pnl:trader:summary` | A trader's day summary recomputes (consumed by Risk's loss ladder) |
| `trade8:pnl:office:summary` | An office roll-up recomputes |
| `trade8:pnl:overnight:mismatch` | A restored overnight order's carried P&L diverges from the position book |

**Consumed:** router fills (positions), feed prints (marks),
desk session-close (carry + persist), desk overnight-restored
(verification).

## 5. Business Rules & Invariants

1. P&L derives only from fills and opening balances - nothing can "adjust"
   positions outside the event flow without leaving a mismatch trail.
2. A fill is applied exactly once regardless of redelivery.
3. Fees convert at the persisted currency rate; unknown rates default to
   1.0 and are flagged in the audit trail.
4. LFT never understates: peaks remain counted after drawdowns within the day.
5. Overnight carry is exact: the next session opens with today's final
   positions and cost basis; divergence is a visible mismatch, never a silent roll.
6. Traders see their own P&L only (self tier); office managers see their
   office; env managers see all.
7. Session close persists everything durably before the next session opens.

## 6. Target-User Roles & Use Cases

| Persona | Use cases |
|---|---|
| Trader | Watch day P&L per symbol and total, track LFT giveback, check position size/cost |
| Office/branch manager | Monitor the office P&L board, spot the desk's best/worst symbols |
| Desk manager | The all-traders board; feed decisions to supervision |
| Risk (system) | Consume summaries to drive loss ladders |
| Back office / ops | Verify overnight continuity; work the mismatch queue |
| Compliance/DR | Replay any day's P&L from fills + balances for audit |

## 7. KPIs / Success Criteria

- Summary freshness: a fill or mark updates the affected boards within the
  fill-processing budget (latency-harnessed in CI).
- Zero money drift: replay of the same fill sequence yields identical
  summaries (differential tests).
- Overnight integrity: zero unexplained carry differences; all mismatches
  surfaced with expected vs actual.
- Fee conversion accuracy pinned at ×10⁴ scale by tests.
- Office roll-ups equal the sum of member trader summaries.

## 8. Dependencies & Reliance on Other Services

| Dependency | Why the business relies on it |
|---|---|
| Trade8:Router | The authoritative fill stream (sole fill writer) |
| Trade8:Feed | The print stream marking unrealized |
| Trade8:Desk | Session-close trigger + overnight snapshots to reconcile |
| Trade8:Risk | The primary consumer of trader summaries (loss ladder) |
| desk.currency | Fee conversion rates |
| Sentin8:Bus | Event transport for inputs and outputs |
| Sentin8:Cron | Session-close scheduling |
| Sentin8:Auth + Shield | Entitlement for self/office/env visibility |

## 9. Acceptance Criteria

1. A trader's P&L board updates within sub-second of each fill/mark.
2. Office and env boards match trader-level numbers by construction (folds,
   not independent sums).
3. After any crash/restart, opening positions match the previous close
   exactly or raise a mismatch.
4. Reported fees always convert at auditable rates.
5. Any trading day can be replayed from events + opening balance to the
   same numbers.
6. All visibility tiers hold under role changes (RLS + tier filters verified).

## 10. Open Business Questions

- Should traders be able to request quote-based (instead of print-based)
  marks per symbol class?
- What tooling should the ops mismatch queue provide (adjust/replay/approve)?
- Does the business need end-of-day export of office summaries to external
  reporting, and in what format?
- What is the retention policy for day summaries (beyond the platform's
  Storage-backed cold tier design)?

---

**Architecture & Design:** trade8:pnl and its underlying concepts were designed by [Hamid Jahanzad](https://www.linkedin.com/in/hamidjahanzad/).
