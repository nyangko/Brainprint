<p align="center">
  <img src="./assets/brainprint-hero.svg" alt="Brainprint" width="100%" />
</p>

<p align="center">
  <strong>A local-first shared context runtime for AI coding agents.</strong>
</p>

<p align="center">
  English · <a href="./docs/README.ko.md">한국어</a> · <a href="./docs/README.ja.md">日本語</a>
</p>

## What is Brainprint?

Brainprint is being built to keep a persistent, fresh understanding of a software project and provide AI coding agents with only the context they need for the current task.

Today, agents often rediscover the same repository structure, symbols, dependencies, decisions, and project rules every time a session changes. Brainprint aims to maintain that understanding continuously so agents can spend less time searching and more time working with the project correctly.

The repository remains the source of truth. Brainprint is not a backup system and does not replace Git. It maintains structured project understanding around the repository.

## Where is it going?

Brainprint is currently in the early design and implementation stage.

The first version will focus on the **code-development workflow**:

1. Build and maintain a fresh index of project resources, symbols, occurrences, and relations.
2. Track changes incrementally instead of rescanning the entire repository.
3. Expose compact context to AI agents through a small, predictable interface.
4. Preserve project decisions, rules, and working state across sessions and agents.
5. Benchmark whether Brainprint actually reduces repeated file reads, tool calls, tokens, and exploration time without reducing correctness.

After the code workflow is proven, the same core model may be extended to documents, images, video, audio, and other project resources without turning the v1 core into a generic knowledge platform too early.

## Current status

**Early development / design validation.**

The architecture and priorities are still being refined through implementation and benchmarks. Interfaces and storage details may change until the first practical code-development workflow is validated.
