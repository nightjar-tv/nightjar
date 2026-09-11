# heldout/

Reserved for cases held out of parser tuning so a later slice can measure
without fitting to the cases.

Ownership: the core crate. Held-out cases must be authored independently of the
`development/` set, before any tuning that they measure. A case never moves into
this root after it was used for tuning.

Do not fabricate held-out cases. No cases are present yet.
