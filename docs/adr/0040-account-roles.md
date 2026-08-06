# ADR-0040: Account roles

- Status: accepted
- Date: 2026-08-06
- Accepted: 2026-08-06, with all nine questions signed off in
  `nightjar-meta/notes/design/adr-0039-0040-questions-2026-08-06.md`.
  **B2-1 must not dispatch until its step table takes the `role` column** — its
  migration writes the account table and this record changes what column it
  takes (sheet Q17)
- Amends: ADR-0034 item 2 (which said there is no role system) under Rule 6.4;
  qualifies ADR-0034 items 3, 7 and 10 as recorded below
- Depends on: ADR-0034 (accounts, profiles, scope, sessions, bootstrap, the
  `reset-password` subcommand). Nothing else, so the schema dependency runs one
  way: this record decides a column on `accounts` and decides no other table.
  Item 8 names ADR-0037's override scope column, which is a statement about what
  each role may write and not a decision about that table's shape
- Gate: Gate 3 — kids scoping and parent overrides are unreachable from a
  profile session including an adult-flagged one; full v1 API frozen
- Related: ADR-0035 item 7 and ADR-0037 items 9, 11 and 12, which read this
  record; Block 2 plan B2-G, B2-1 and B2-2
  (`nightjar-meta/docs/BLOCK_2_PLAN.md`); decision sheet
  `nightjar-meta/notes/design/adr-0039-0040-questions-2026-08-06.md`;
  ADR-0035 to ADR-0038 sign-off sheet Q22, which raised this

## Context

ADR-0034 item 2 said: "No role system. Owner and member is the whole model.
`can_manage_server` is a boolean on the account, not a row in a roles table and
not a permission set (Rule 4.7). A second axis of authority arrives with a
second concrete use case or not at all."

That was the right call on the evidence available, and item 2's own sentence
named the condition under which it would change. The condition is met, so this
record reports that the second use case arrived rather than that the rule was
wrong. Rule 4.7 stays intact for the next proposal that wants a general
permissions system with nothing to point at.

**The second use case is eviction.** Under a boolean, any account with
`can_manage_server` can delete any other account with it, and ADR-0034 item 7
protects only the last one. So the person who installed the server, added the
libraries, and holds the data directory can be removed from their own server by
someone they trusted with a scan button. A household with two adults who both
need to add a library is the ordinary case, not a hypothetical, and a boolean
cannot express "you may administer this server and you may not evict me".

**Three routes then arrived needing a middle answer**, each written and signed
off before this record existed, each with the same shape — the whole server, or
one account's own people, and nothing in between:

- ADR-0035 item 7: which `profileRef`s a caller may address.
- ADR-0037 item 11: whose caps `GET /system/kids-scope` reports against.
- ADR-0037 item 12: whose viewing history is readable from account scope.

Under two values, a second family sharing the server either administers
everything or administers nothing, including their own children's profiles.

## Decision

**Three roles on the account — owner, manager, member — as one closed-enum
column. There is no roles table, no permission set, and no per-row precedence
field.**

### 1. The roles

| Role | Reach |
|---|---|
| **owner** | Everything. Exactly one per server, cannot be removed, and is the only role that can perform the owner-only actions in item 3 |
| **manager** | Full account powers across the server, except the owner-only actions |
| **member** | Themselves and the profiles under their own account, and nothing else |

"Full account powers" means what a manager can do to other accounts: create
them, delete member accounts, create and delete profiles under them, set their
caps and simple-interface flags, reset their passwords, and revoke their
sessions. It does not mean role changes, which are item 3.

A member's reach is stated as a boundary rather than a list, because the list
grows: a member may act on their own account — password, and creating, renaming
and deleting profiles under it, including setting their caps and
simple-interface flags — and every other account is a named forbidden error.

Profile management is the whole point of the role. A member with a child's
profile under their account is a parent, and a parent who cannot set their own
child's cap has no reason to hold an account.

### 2. Storage is one column, not a table

`accounts` carries `role TEXT NOT NULL CHECK (role IN ('owner','manager',
'member')) DEFAULT 'member'`, replacing `can_manage_server`. There is no
`roles` table and no `permissions` table.

**Exactly one owner is a constraint, not a convention:**

```sql
CREATE UNIQUE INDEX idx_accounts_one_owner ON accounts (role) WHERE role = 'owner';
```

A partial unique index means two owners cannot exist even through a code path
nobody reviewed, which is the same instinct as ADR-0037 item 7's
non-defaultable scope parameter: the guarantee is that the write fails, not
that a reviewer notices.

**This lands before B2-1, not after.** B2-1's migration is written and has not
run, so `can_manage_server` has never existed on disk and there is nothing to
migrate. If B2-1 ships first, the column ships wrong and a second migration
undoes it in the same block. The Block 2 plan carries this ordering.

### 3. Owner-only actions, and they are exactly three

1. Transfer ownership to another account.
2. Change any account's role.
3. Delete an account whose role is `manager`.

Everything else a manager may also do. The list is short on purpose: these are
the three actions that would otherwise deadlock or allow eviction, and every
other administrative action is safe in a manager's hands because the owner can
reverse it.

**Transfer ownership** demotes the acting owner to `manager` and promotes the
target in one transaction, so the partial unique index is never violated and
the server is never ownerless. The target must be an existing account. There is
no unowned state and no "no owner" migration path.

**Recovery when the owner account is lost** is ADR-0034 item 12's
`nightjar reset-password <username>` on the server, which already requires the
filesystem access that is already the trust boundary. This record adds no
subcommand. An owner is unremovable, so there is no case where the owner row is
gone and only its password is unreachable.

### 4. "The owner tie-breaks" is the action list, not a precedence field

Two managers can disagree — one allows a title in kids mode, the other removes
it. The mechanism that resolves it is that the owner can change roles and
remove a manager, and a manager cannot do either. There is no priority column
on the override table, no "set by role" ordering, and no rule that an owner's
row outranks a manager's row.

Adding a precedence field would put an authority axis on every table that any
role can write, to serve a dispute the owner can already end. That is the
speculative abstraction Rule 4.7 names, and it would also give ADR-0037 item 6
a second ordering to disagree with. Last write wins within a table; the owner
wins by removing the writer.

### 5. Role is an account property, scope is a session property, admin needs both

ADR-0034 item 3 stands: authority is account scope, a profile session never
carries authority, and widening back to account scope re-authenticates. This
record changes only what "admin" tests.

An administrative route requires **account scope and a sufficient role**. A
member in account scope is authenticated and is not an administrator, so
`can_manage_server` in ADR-0034 item 3 and in B2-1 step 5's shared extractor
reads as "role is owner or manager".

**ADR-0034 item 3's unrestricted browse is a manager-and-owner capability.**
Its stated reason is that the manual fix flow has to see the item it is
fixing, and a member cannot use the fix flow. A member's account scope
therefore reaches account and profile management and does not reach
item-returning routes at all: those are refused at the route layer with the
named forbidden error before any query runs.

That refusal is a route-level role check, not a third viewer scope. ADR-0037
item 7's scope parameter stays two-valued — account browses unrestricted,
profile carries the cap — so the type-level guarantee that a new route cannot
compile without deciding its scope is untouched. Had a member's account scope
instead browsed unrestricted, an adult member could hand a capped child their
own account password and the cap would be one login away, which is the failure
ADR-0034 item 3 exists to prevent.

### 6. Account creation is the whole mechanism; there are no invites

An owner or a manager creates an account, which is the only way an account
comes into existence after bootstrap. There is no invitation token, no email,
no self-registration route, and no pending-account state.

An owner may create an account as `member` or `manager`. A manager may create
`member` accounts only, because granting `manager` is a role change and role
changes are owner-only (item 3). A member creates no accounts, and creates
profiles only under their own (item 1).

Accounts are not self-deleted. Removal is an owner or manager action, and for a
manager account it is owner-only. Self-deletion would be a fourth path to a
state the other three already reach and would need its own answer for the owner
case, which cannot be deleted at all.

### 7. Bootstrap creates the owner

ADR-0034 item 10 is unchanged in mechanism and named in outcome: the one
unauthenticated write, gated only on "no account exists", creates the first
account with role `owner`. The bootstrap window is the same accepted posture
and stays on the security-pass list.

ADR-0034 item 7's "the last account with `can_manage_server` cannot be deleted
or have the toggle cleared" is replaced by a simpler statement: the owner
cannot be deleted and its role changes only through transfer. The count-based
rule existed because a boolean has no singleton; a role column plus item 2's
index has one.

### 8. Overrides are scoped by the role that writes them

ADR-0037 item 9 ships an override scope column defaulting to server-wide, and
the sign-off sheet Q24 recorded that no v1 surface sets it to anything else.
The role model supplies the surface, so that consequence changes here:

- An owner or a manager may write a **server-wide** override, and may write an
  account-scoped one.
- A member may write an override scoped to **their own account only**. It
  applies to the profiles under that account and to no others, and it defaults
  to that scope rather than to server-wide.

Without this a member cannot allow a title for their own child without asking
another household's adult, which defeats the role this record exists to create.
With it, the column Q24 shipped has a v1 writer and a v1 meaning, which is a
better outcome than a column no route sets.

Everything else in ADR-0037 item 9 is unchanged: both overrides are
account-scope actions, PIN-confirmed, series-scoped for shows via the ADR-0039
`series_key`, and never available from a profile session including an
adult-flagged one.

### 9. Role appears on the wire and is never inferred

Account responses carry `role` as a closed string enumeration, and a client
renders affordances from it rather than probing routes to discover what it may
do. The server enforces regardless; the field exists so a manager is not shown
a transfer-ownership button that will return a forbidden error.

Forbidden is a named error distinct from unauthenticated and distinct from
unknown, matching ADR-0034 item 5's discipline of keeping failure modes as
separate typed errors instead of collapsing them into one.

## Alternatives considered

**Keep the boolean and add a second boolean, `is_owner`.** Two booleans express
three states in four slots, so `is_owner` without `can_manage_server` is a
representable nonsense state that every reader must handle or ignore. A closed
enum has exactly three values and the database rejects the fourth.

**A roles table with a permission set.** The general answer, and the one
ADR-0034 item 2 rejected. Still rejected, and this record does not build it:
three closed values on the account are not a permissions system, and the moment
somebody wants a fourth they need a third concrete use case, not a join table.

**Manager may change roles, so a household is not blocked when the owner is
away.** Rejected: a manager who can grant `manager` can multiply managers, and
only the owner can un-multiply them, so the owner's absence becomes a worse
problem rather than a smaller one. The escape hatch is `reset-password` on the
server, which is already the answer to every other lost-credential case.

**Per-row precedence so an owner's override outranks a manager's.** Rejected in
item 4. It puts an authority column on every writable table and gives ADR-0037
item 6's single precedence order something to disagree with.

**A member's account scope browses unrestricted, matching ADR-0034 item 3
literally.** Rejected in item 5: it makes a cap one account password away for
the exact household shape — one server, several families — that the member role
exists to serve.

**Invitations.** Rejected under Rule 4.7 and Rule 4.12. Account creation by an
administrator is already a complete mechanism, an invite adds a token lifetime,
a pending state, and a delivery channel the server does not have, and nothing
in v1 needs a person to create their own account.

## Consequences

**Good**

- ADR-0035 item 7, ADR-0037 item 11 and ADR-0037 item 12 can be accepted; all
  three were written against this model and blocked on it.
- The person who installed the server cannot be evicted from it, which the
  boolean could not express.
- A second family on one server administers their own people and nothing else,
  which is what makes sharing a server tolerable.
- ADR-0037's override scope column gains a v1 writer instead of shipping unset.

**Bad (accepted)**

- ADR-0034 is amended the same day it was accepted, and item 2's prose has
  to be read through this record. That is the cost of writing item 2 honestly
  rather than hedging it, and the amendment says so in place (Rule 6.1). It also
  says something about the sequencing: item 2 was written before B2-B to B2-E,
  and those four records are what produced the counter-evidence.
- Three roles is more surface than a boolean: every administrative route now
  answers "which roles", the answer is in this ADR rather than in one extractor,
  and B2-1's admin extractor takes a role rather than reading a flag.
- A member cannot be given one extra power without becoming a manager. There is
  no partial grant, by design, and the household's answer is to make them a
  manager or not.
- Ownership transfer is a route that runs rarely and must be correct, since
  getting it wrong either leaves two owners, which the index refuses, or none,
  which the transaction prevents. It needs its own test rather than inheriting
  one.

**Open, and deliberately not decided here**

- The PIN remains B2-7's, unbound here and unbound by ADR-0034 item 3 and
  ADR-0037 item 9.
- Any surface that lets a user choose an override's scope explicitly is Block 3.
  Item 8 decides the default and the ceiling per role; it does not design a
  picker.
- Whether a manager can see another account's login-session list is not decided;
  no v1 route lists another account's sessions, and the question arrives with
  the route.
