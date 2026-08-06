let meta = #{
    name: "plan-implement-review-hard-gate",
    description: "Enforces the agent pipeline with hard gates: plan → implement → verify → human publish. Fails fast on violations."
};

// Input: plan_path, pr_number, repo_url
let args = args;

// Step 1: Validate plan exists and has acceptance criteria
let plan_path = args.plan_path;
let plan_file = read_file(plan_path);

if plan_file == "" {
    return #{
        success: false,
        message: "Plan file not found: " + plan_path
    };
}

// Check for acceptance criteria in plan
if !plan_file.contains("acceptance") {
    return #{
        success: false,
        message: "Plan missing acceptance criteria. Every step must have acceptance criteria."
    };
}

// Step 2: Spawn implementer with strict rules (dispatch contract: slice id + plan path only)
let implementer_prompt = "Implement step " + args.step_id + " from the plan at " + plan_path + ".\n"
    + "Read the plan file for your step; the plan is authoritative. Rules in your system prompt.\n"
    + "Green before done: acceptance commands, cargo test, cargo clippy --workspace --all-targets -- -D warnings, cargo fmt --check.\n"
    + "Fix red gates locally. No redesign, no scope expansion, no new deps without ADR. Working tree only. Report form required.";

let implementer = agent("deepseek-flash", implementer_prompt, {
    model: "deepseek-flash",
    capability_mode: "read-write",
    default_fork_context: false
});

// Wait for implementer to complete
let implementer_result = get_command_or_subagent_output([implementer]);

if !implementer_result.success {
    return #{
        success: false,
        message: "Implementer failed: " + implementer_result.message
    };
}

// Step 3: Spawn independent verify agent (green-first: gates before review)
let verify_prompt = "GREEN FIRST: re-run acceptance commands, cargo test, cargo clippy --workspace --all-targets -- -D warnings.\n"
    + "Red gates = immediate FAIL with only the red commands as issues (mechanical round, not a review round).\n"
    + "Only on a green tree: check acceptance criteria, no rule violations, no redesign/scope expansion,\n"
    + "working tree only (no commit/push/PR). Report issues in structured checklist format.";

let verify_agent = agent("grok-4.5", verify_prompt, {
    model: "grok-4.5",
    capability_mode: "read-only",
    default_fork_context: false
});

// Wait for verify agent to complete
let verify_result = get_command_or_subagent_output([verify_agent]);

if !verify_result.success {
    return #{
        success: false,
        message: "Verify agent failed: " + verify_result.message
    };
}

// Step 4: Check if verify passed
if verify_result.message.contains("FAIL") {
    return #{
        success: false,
        message: "Verify failed. Issues found: " + verify_result.message
    };
}

// Step 5: Check if SLICE_CLOSEOUT.md is completed
let closeout_path = "../nightjar-meta/docs/SLICE_CLOSEOUT.md";
let closeout_file = read_file(closeout_path);

if closeout_file == "" {
    return #{
        success: false,
        message: "SLICE_CLOSEOUT.md not found. Must be completed before merge."
    };
}

// Check if all questions are answered
if !closeout_file.contains("Yes") {
    return #{
        success: false,
        message: "SLICE_CLOSEOUT.md not completed. All questions must be answered."
    };
}

// Step 6: Success
return #{
    success: true,
    message: "Agent pipeline passed. All gates cleared. Ready for human merge."
};