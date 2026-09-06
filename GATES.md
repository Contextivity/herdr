# Gates: native agent-provider runtime and sidebar

OWNS: src/api/**, src/app/**, src/ui/**, tests/**, docs/next/**, GATES.md

Scope: let a source register bounded data-only agents that render and select through the native sidebar using one shared viewer pane, without changing physical agent.list semantics.

- [x] G1: source-owned provider snapshots validate bounds, reject stale revisions, replace atomically, and clear authoritatively
  CHECK: cargo test --bin herdr provider_agent_registry_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider registry: PASS
  EXPECT: herdr provider registry: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider registry: PASS

- [x] G2: provider records stay out of physical agent.list and survive local view filtering without recursive fleet discovery
  CHECK: cargo test --bin herdr provider_agent_inventory_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider inventory: PASS
  EXPECT: herdr provider inventory: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider inventory: PASS

- [x] G3: provider agents use the existing orchestration cards, lifecycle colors, ordering, scrolling, and 500-agent layout path
  CHECK: cargo test --bin herdr provider_agent_sidebar_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider sidebar: PASS
  EXPECT: herdr provider sidebar: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider sidebar: PASS

- [x] G4: mouse, keyboard, and API focus select exactly one provider identity while focusing its shared viewer pane and emitting a source/id event
  CHECK: cargo test --bin herdr provider_agent_focus_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider focus: PASS
  EXPECT: herdr provider focus: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider focus: PASS

- [x] G5: provider removal or viewer loss clears selection safely without closing or mutating any remote agent
  CHECK: cargo test --bin herdr provider_agent_cleanup_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider cleanup: PASS
  EXPECT: herdr provider cleanup: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider cleanup: PASS

- [x] G6: provider methods, transient-state semantics, focus events, and capability negotiation are schema-covered for projector reconciliation
  CHECK: cargo test --bin herdr provider_agent_protocol_ -- --test-threads=1 >/dev/null 2>&1 && echo herdr provider protocol: PASS
  EXPECT: herdr provider protocol: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr provider protocol: PASS

- [x] G7: the full Herdr repository contract passes on the final tree
  CHECK: just check >/dev/null 2>&1 && echo herdr checks: PASS
  EXPECT: herdr checks: PASS
  EVIDENCE: exit=0; shell=/bin/sh; cwd=/Users/renefranke/Development/Repos/herdr-worktrees/native-agent-provider; path=4a744a5ebee9/37 entries; output=herdr checks: PASS
