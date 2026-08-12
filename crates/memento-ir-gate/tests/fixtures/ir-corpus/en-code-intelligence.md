# Code intelligence: searching symbols across a Rust codebase

This document describes how a code intelligence tool indexes and
searches symbols across a Rust codebase. The goal is to answer
questions like "where is this function defined?" and "who calls this
symbol?" quickly, even in large repositories.

## Symbols and their relationships

A codebase is more than a collection of files: it is a graph of
symbols. Each function, struct, trait, enum and macro is a symbol.
Symbols have definitions, call sites and dependencies. An index built
over those symbols turns code search into a graph query instead of a
text search.

## Building the symbol index

The index pipeline has four stages:

1. Parse each source file into an abstract syntax tree.
2. Extract every symbol with its name, kind and location.
3. Resolve cross-references between symbols.
4. Store the result in an inverted index and a symbol table.

Once the index is built, a query like "callers of the search function"
resolves in milliseconds instead of scanning the whole tree.

## Searching by symbol

Code search over symbols is more precise than grep. Searching for the
string "embed" matches comments, strings and variables; searching for
the symbol `embed` matches only the function, method or trait that
owns that name. The symbol index answers the question the developer
actually asked.

## Incremental updates

The index must stay fresh as the codebase changes. Incremental
reindexing processes only the modified files, updates the affected
symbols and re-resolves their references. Without incrementality, the
index decays and developers stop trusting it.

## Conclusion

Symbol indexing turns a codebase into a queryable knowledge graph.
Functions, callers and dependencies become first-class citizens of the
search experience. A well-built symbol index is the foundation of
every modern code intelligence tool.
