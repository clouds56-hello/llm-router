---
name: repo-reference
description: 'Reference external repository implementations during planning and implementation. Use when requests include tokens like ~owner/repo, when you need to clone or update a repo mirror, inspect DeepWiki pages, compare behavior, and map proven patterns into the current repository plan.'
argument-hint: 'Provide one or more repo tokens such as ~openai/codex plus the feature, flow, or subsystem to study.'
user-invocable: true
disable-model-invocation: false
---

# Repo Implementation Reference

## What This Skill Does

Use this skill when a task should be informed by another repository's implementation, especially when the prompt includes shorthand tokens such as `~openai/codex`.

The skill turns those references into a repeatable workflow:

1. Resolve each `~owner/repo` token into a concrete source repository.
2. Create or reuse local cache state for code, wiki pages, and durable notes.
3. Search the reference repo for the specific feature, API shape, workflow, or architectural pattern relevant to the current task.
4. Read only the code paths and wiki pages that matter.
5. Map those findings into the current repository's plan before editing.

## Skill Assets

- DeepWiki bulk exporter: [scripts/deepwiki-scraper.py](./scripts/deepwiki-scraper.py)
- Dependency bootstrapper: [scripts/run-deepwiki-scraper.sh](./scripts/run-deepwiki-scraper.sh)
- Python dependencies: [scripts/requirements.txt](./scripts/requirements.txt)

Use the scraper only when the user explicitly wants a full DeepWiki export or refresh. It is not the default path for normal reference lookups because the default wiki policy is page-by-page caching.

## When To Use

- The user asks to "reference" or "follow" another repo's implementation.
- The request includes one or more `~owner/repo` tokens.
- The task benefits from comparing code structure, request flow, configuration shape, API contracts, or UI patterns against an upstream or sibling project.
- You need concrete prior art before deciding how to implement a feature in the current repo.

## Default Cache Layout

Store per-repository cache state under `.cache/repos/<owner>__<repo>/` inside the current workspace unless the user specifies another location.

Use this structure:

- code: `.cache/repos/<owner>__<repo>/code`
- wiki: `.cache/repos/<owner>__<repo>/wiki`
- memory: `.cache/repos/<owner>__<repo>/memory`

This keeps the reference repos project-scoped, inspectable by the user, reusable across tasks, and separate from the main source tree.

## Procedure

1. Parse the request for tokens matching `~owner/repo`.
2. For each token, define:
	- the GitHub slug: `owner/repo`
	- the repo cache root: `.cache/repos/<owner>__<repo>`
	- the code mirror path: `.cache/repos/<owner>__<repo>/code`
	- the wiki cache path: `.cache/repos/<owner>__<repo>/wiki`
	- the local memory path: `.cache/repos/<owner>__<repo>/memory`
	- the specific behavior to study
3. Before editing the current repo, gather the reference context.
4. If the code mirror path does not exist, clone it with a shallow, non-destructive command such as:

```bash
git clone --filter=blob:none --depth 1 https://github.com/owner/repo.git .cache/repos/owner__repo/code
```

5. If the code mirror already exists, refresh it only when the user explicitly asks for an update, a refresh is clearly required, or the task depends on the newest upstream state. Refresh it without rewriting local work:

```bash
git -C .cache/repos/owner__repo/code remote update --prune
git -C .cache/repos/owner__repo/code pull --ff-only
```

If no refresh was requested and the existing mirror is good enough for the task, reuse it as-is and say so in the working notes.

6. If you need a small number of DeepWiki or similar documentation pages for the reference repo, fetch only the pages you actually read and cache them under `.cache/repos/<owner>__<repo>/wiki`.
7. If the user explicitly asks for a full DeepWiki snapshot, use the dependency-managed wrapper script:

```bash
.agents/skills/repo-reference/scripts/run-deepwiki-scraper.sh owner/repo .cache/repos/owner__repo/wiki
```

8. Record durable local notes for the task under `.cache/repos/<owner>__<repo>/memory`, such as:
	- paths already inspected
	- behaviors already confirmed
	- repo-specific caveats
	- open questions worth revisiting later
9. Search the reference repo for the exact subsystem named in the task. Prefer targeted file and text searches over broad scans.
10. Read only the files required to understand:
	- entry points
	- request and response flow
	- configuration structures
	- data models
	- tests or examples that lock in intended behavior
11. Extract the implementation logic into a concise working note for the current task:
	- what the reference repo does
	- which files implement it
	- what assumptions or dependencies it relies on
	- what is portable versus repo-specific
12. Convert the findings into the current repo's plan:
	- identify analogous files or modules here
	- call out mismatches in architecture or constraints
	- propose an implementation approach grounded in the reference
13. Only then edit code in the current repository.

## Dependency Setup

Do not assume the host Python environment already has the DeepWiki scraper dependencies.

- Use [scripts/run-deepwiki-scraper.sh](./scripts/run-deepwiki-scraper.sh) instead of invoking the Python scraper directly.
- The wrapper creates a dedicated virtual environment under `.cache/skills/repo-reference/.venv`.
- The wrapper installs or refreshes dependencies from [scripts/requirements.txt](./scripts/requirements.txt) before running the scraper.
- Keep dependency management isolated from the main repository toolchain.

## Decision Points

### When to clone versus use remote-only inspection

- Clone or use an existing local code mirror when the task needs multi-file understanding, repeated lookups, or path-level citations.
- Refresh an existing mirror only on request or when stale upstream state would materially affect the answer.
- Use lightweight remote inspection only when a fast one-off lookup is enough or network and filesystem constraints prevent cloning.

### When to cache wiki pages

- Cache a wiki or DeepWiki page only after reading it.
- Do not bulk download docs in advance.
- Reuse cached pages when they are sufficient for the task.
- Refresh or replace a cached page only when the source changed or the cached page is incomplete.
- Use the bundled scraper only for explicit full-export requests.

### When to use one reference repo versus several

- Use one repo when there is a clear upstream or canonical implementation.
- Use multiple repos when the user explicitly compares approaches or when one repo covers only part of the workflow.

### What to extract

Extract behavior, structure, and constraints. Do not blindly port code. Preserve:

- flow of control
- API and config shape
- guardrails and error handling
- testable behavior

Avoid carrying over:

- repo-specific abstractions that do not fit the current codebase
- unrelated dependencies
- framework conventions that conflict with the target repo

## Planning Requirements

When this skill is used, the plan for the current task should include a short "Reference repo findings" section covering:

- referenced repos
- exact files inspected
- any wiki pages read or exported
- the implementation pattern chosen
- deviations required in the current repo

If no usable prior art is found, state that explicitly and continue with a first-principles implementation plan.

## Quality Checks

Before finishing, verify that:

- every `~owner/repo` token was resolved correctly
- the code mirror is present or the fallback inspection method is documented
- wiki pages were cached only for pages actually read unless the user asked for a full export
- local memory was updated only with durable, task-relevant notes
- extracted findings cite concrete files, not vague summaries
- the implementation plan distinguishes copied ideas from adapted design
- proposed changes fit the current repo's architecture rather than mirroring the reference blindly

## Completion Criteria

This skill is complete for a task when:

1. The referenced repo or repos have been cloned or refreshed under the `code` cache, or a documented fallback inspection path was used.
2. The relevant implementation files have been identified and read.
3. Any wiki pages consulted were cached under the `wiki` cache without bulk prefetching, unless the user explicitly asked for a full export.
4. The current repo plan includes a grounded summary of what to reuse, what to adapt, and what to ignore.
5. Any eventual code changes in the current repo can be traced back to concrete reference findings.

## Example Prompts

- `/repo-reference Use ~openai/codex to shape the plan for our auth refresh flow.`
- `/repo-reference Compare ~openai/codex and ~anthropics/claude-code for streaming response handling, then map the result into this repo.`
- `Use ~openai/codex as prior art before changing our provider routing logic.`
- `Export the full DeepWiki for ~openai/codex into the wiki cache so we can inspect it offline.`

## Notes

- Keep reference work read-only unless the user explicitly asks to modify the mirrored repo.
- Prefer non-destructive git commands.
- Cite the reference repo's concrete file paths in your summary so the user can validate the reasoning.
