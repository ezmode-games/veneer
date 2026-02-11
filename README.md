# veneer

Rust-powered component documentation generator with live Web Component previews.

## Overview

`veneer` transforms your component library documentation from MDX files into a static site with interactive previews. Unlike traditional documentation tools that require a JavaScript runtime for component previews, veneer transforms React/Solid JSX into static Web Components that work without any framework.

**Key Features:**

- **Zero-runtime previews** - Components render as Web Components with Shadow DOM
- **Hot reload** - Instant updates during development via WebSocket HMR
- **Fast builds** - Parallel processing with Rayon for large documentation sites
- **Framework agnostic** - Transform React or Solid components to static previews
- **Tailwind-ready** - `adoptedStyleSheets` API brings styles into Shadow DOM

## Quick Start

### Installation

```bash
# Install via script
curl -fsSL https://raw.githubusercontent.com/ezmode-games/veneer/main/install.sh | bash

# Or build from source
cargo install --path crates/veneer
```

### Initialize Documentation

```bash
# In your project root
veneer init

# This creates:
# - docs.toml          Configuration file
# - docs/              Documentation directory
# - docs/index.mdx     Welcome page
```

### Development Server

```bash
veneer dev

# Opens http://localhost:7777 with hot reload
```

### Build Static Site

```bash
veneer build

# Output: dist/
```

### Preview Built Site

```bash
veneer serve

# Serves dist/ on http://localhost:4000
```

## Writing Documentation

### MDX Format

````mdx
---
title: Button
description: A versatile button component
order: 1
---

# Button

The Button component supports multiple variants and sizes.

## Basic Usage

```tsx preview
<Button variant="primary">Click me</Button>
```

## Variants

```tsx preview
<div className="flex gap-2">
  <Button variant="default">Default</Button>
  <Button variant="primary">Primary</Button>
  <Button variant="destructive">Destructive</Button>
</div>
```
````

### Code Block Modes

- **`static`** (default) - Syntax highlighted code only
- **`preview`** - Live Web Component preview + code
- **`live`** - Interactive preview (future)

### Frontmatter Options

| Field | Type | Description |
|-------|------|-------------|
| `title` | string | Page title |
| `description` | string | SEO description |
| `order` | number | Navigation order |
| `nav` | boolean | Show in navigation (default: true) |

## Configuration

### docs.toml

```toml
[docs]
dir = "docs"
output = "dist"
title = "My Component Library"
base_url = "/"
styles = ["path/to/tokens.css", "path/to/utilities.css"]

[components]
dir = "src/components"

[build]
minify = true
```

## Architecture

```
veneer/
├── veneer-mdx        MDX parsing and code block extraction
├── veneer-adapters   JSX to Web Component transformation
├── veneer-static     Static site generation
├── veneer-server     Development server with HMR
└── veneer            CLI orchestration
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for detailed technical documentation.

## Requirements

- Rust 1.75+ (uses edition 2021)
- Components using Tailwind CSS (for style adoption)
- React or Solid component syntax

## License

MIT
