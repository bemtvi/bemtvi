<!--
  This is the TEMPLATE for the book's table of contents.

  `book/gen/generate.py` renders it into src/SUMMARY.md, replacing the
  {{API_REFERENCE}} marker below with the auto-generated nx.* namespace list.
  Edit THIS file (committed), never src/SUMMARY.md (git-ignored, regenerated).

  Pages under guide/ and plugins/overview are committed curated chapters.
  Pages marked "(imported)" are copied from docs/*.md by the generator with
  their relative links rewritten to GitHub URLs — also git-ignored.
-->

# Summary

[Introduction](introduction.md)

# User Guide

- [Getting started](guide/getting-started.md)
- [Configuration](guide/configuration.md)

# Beyond vim

- [What nxvim adds](features/index.md)
  - [Multi-cursor mode](features/multicursor.md)
  - [UI primitives](features/ui-primitives.md)
  - [Permanent docks](features/docks.md)
  - [Fuzzy picker](features/picker.md)
  - [Browser editor](features/browser-editor.md)
  - [The edit-host split](features/edit-host-split.md)

# Plugin Development

- [Overview](plugins/overview.md)
- [The nx.* model](plugins/nx-model.md)
- [Writing plugins](plugins/authoring.md)
- [Async & promises](plugins/async.md)
- [Testing plugins](plugins/testing.md)
- [Autocommand events](plugins/autocmd-events.md)

# nx.* API Reference

- [Overview](api/index.md)
{{API_REFERENCE}}

# Architecture

- [Design & internals](architecture/overview.md)

# Appendix

- [Known approximations](appendix/known-approximations.md)
- [Verifying downloads](appendix/verifying-downloads.md)
