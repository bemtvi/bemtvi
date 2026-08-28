<!--
  This is the TEMPLATE for the book's table of contents.

  `book/gen/generate.py` renders it into src/SUMMARY.md, replacing the
  {{API_REFERENCE}} marker below with the auto-generated btv.* namespace list.
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
- [Recommended plugins](guide/recommended-plugins.md)

# Beyond vim

- [What bemtvi adds](features/index.md)
  - [Helix mode (selection-first)](features/helix-mode.md)
  - [Multi-cursor mode](features/multicursor.md)
  - [Keyboard macros](features/macros.md)
  - [Expressions](features/expressions.md)
  - [Smooth scrolling](features/smooth-scrolling.md)
  - [Indent detection](features/indent-detection.md)
  - [Image previews](features/image-previews.md)
  - [UI primitives](features/ui-primitives.md)
  - [Permanent docks](features/docks.md)
  - [Fuzzy picker](features/picker.md)
  - [Quickfix & named-list dock tabs](features/quickfix-dock-lists.md)
  - [Workspaces](features/workspaces.md)
  - [Browser editor](features/browser-editor.md)
  - [The edit-host split](features/edit-host-split.md)

# Plugin Development

- [Overview](plugins/overview.md)
- [First-party plugins](plugins/first-party.md)
- [The btv.* model](plugins/btv-model.md)
- [Writing plugins](plugins/authoring.md)
- [Async & promises](plugins/async.md)
- [Testing plugins](plugins/testing.md)
- [Autocommand events](plugins/autocmd-events.md)

# btv.* API Reference

- [Overview](api/index.md)
{{API_REFERENCE}}

# Architecture

- [Design & internals](architecture/overview.md)

# Appendix

- [Known approximations](appendix/known-approximations.md)
- [Verifying downloads](appendix/verifying-downloads.md)
