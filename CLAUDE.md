## Automatic Development Workflow

For every user message, determine whether it requests actual code changes.

- Questions, discussions, explanations, brainstorming, and review-only requests
  must be handled normally.
- Actual implementation, bug fixing, feature additions, and refactoring
  must invoke the `ai-dev-workflow` skill.
- If the intent is ambiguous, ask the user before starting.

For internal workflow subprocesses marked with AI_DEV_FLOW_ACTIVE=1,
do not recursively invoke the workflow.

The workflow has exactly three stages:
Kimi K3 planning -> one GLM-5.3 implementation agent ->
one Kimi K3 final review.

