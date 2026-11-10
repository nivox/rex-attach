# Code quality

Use metrics to identify review targets and compare changes, alongside code review
and behavioral validation. Prefer trends in changed production code over total
repository scores.

- **Complexity:** Review cognitive complexity (nesting and reading effort) and
  cyclomatic complexity (independent paths). A cognitive score above 15 is a
  prompt to inspect responsibilities, not an automatic reason to split a function.
- **Size and cohesion:** Look at function length, module size, duplication, and
  coupling. Functions over roughly 80 lines deserve review; extract code when it
  clarifies a responsibility or boundary rather than merely reducing line counts.
- **Test coverage:** Look for untested changed paths, especially errors,
  cancellation, cleanup, and protocol boundaries. Distinguish line, region, and
  branch coverage; a high aggregate percentage does not prove correct behavior.
- **Performance:** For renderer changes, compare serial release measurements of
  median and p95 render time, CPU per update, and output bytes for sparse and heavy
  workloads. Preserve dirty-row rendering, cell widths, cache invalidation, and
  bounded image work; use the existing emulator comparisons and benchmarks.
- **Correctness signals:** Keep formatting, tests, and all-target Clippy clean.
  Exercise relevant disposable-session smoke scenarios for ownership, protocol
  ordering, navigation, and graphics when a local Rex server is available.

Compare like-for-like tool versions and workloads. Inspect parser limitations for
macros and async syntax, separate production code from tests, and avoid treating
complexity or coverage as a standalone quality score.
