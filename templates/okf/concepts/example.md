---
type: concept
title: Example concept
description: A starter concept showing OKF frontmatter and markdown links.
tags: [example, getting-started]
timestamp: 2026-01-01
---

# Example concept

This file is a single **concept**. Its identity is its path
(`concepts/example.md`), and its one required frontmatter field is `type`.

Link to other concepts with plain markdown links — the link target is a
relative path to another `.md` file:

- back to the [index](../index.md)
- to a [sibling concept](another.md) (create it to resolve this link)

COGS indexes these links into a `LINKS_TO` graph edge. Add `description`,
`resource`, `timestamp` and `tags` to make the concept richer; all of them are
queryable.
