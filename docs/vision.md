# semnav — Vision

## What we want to achieve

When an AI coding agent understands a codebase, we want to realize a world where **"which code should be read" is identified from structure alone, and code text is read only for the necessary Range**. This drastically reduces the agent's exploration cost and token consumption.

## Current challenges

To understand a codebase, AI coding agents repeatedly use grep, file reads, LSP, and embedding search. But in practice:

* The cost of **exploring which code to read** is larger than the cost of "reading" the code itself
* **Token consumption is large** because whole files are passed to the LLM

Meanwhile, LSP holds rich semantic information — Definition / References / Type Definition / Implementation / Call Hierarchy / Document Symbols, and more. However, its API is query-based, and **there is no interface that "returns the semantic graph of the entire repository"**.

## Solution

Provide a Semantic Graph that **persistently caches** LSP results, as an MCP server.

* Agents query the **Graph, not LSP** (retrieving structure only)
* Code text is read **by specifying a Range**, only when actually needed
* The Graph is a cache, not the Source of Truth. When it goes stale, it is re-evaluated via LSP

## What success looks like

* **Reduced exploration tokens** — structural exploration no longer requires reading whole files
* **Only the necessary Range is fetched** — code text is limited to the minimum lines needed
* **Fewer LSP queries** — the cache absorbs repeated queries
* **Fast structural exploration** — a single graph query suffices
* **Integration of static and dynamic analysis** — runtime observation complements dynamic dispatch (future)
* **Broad support for LSP-supported languages** — semantic analysis is delegated to the existing LSP, so any language with an LSP can be supported

## Core philosophy

The Graph is not "truth" but a "**persistent cache of LSP query results**." This tradeoff reduces the burden of consistency — even if the Graph is stale, LSP can rescue it.
