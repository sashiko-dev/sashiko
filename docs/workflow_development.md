# Developing New Workflows

When adding support for a new codebase or domain (like adapting the Linux Kernel workflow to QEMU or LLVM), creating the multi-stage pipeline is only half the battle. Without domain-specific optimizations, the LLM will thrash on context retrieval. Follow this methodology when developing new reviewer workflows:

1. **Find Historical Bugs/Fixes Pairs:** 
   Identify historical commits that introduced bugs, paired exactly with the commits that fixed them. Distill the fixes into ground-truth "problem descriptions".

2. **Build a Benchmark:** 
   Create a JSON manifest structured with these pairs (e.g., `benchmarks/my_project/benchmark.json`).

3. **Develop, Test, Debug, & Optimize (Micro-Scale):** 
   Select a tiny, representative subset (e.g., 5 patches) to run locally at low concurrency.
   * **Domain Prompts:** Ensure the parallel stages (e.g., Stages 1-7) apply strict domain-specific prompt rules (analogous to the Linux `kernel/subsystem/*.md` definitions).
   * **Prefetcher Customization:** This is critical! Audit your `src/worker/prefetch.rs` logic. Ensure AST prefetch filters map to your project's architecture:
     - Check regex patterns (e.g., parsing QOM macros like `OBJECT_DECLARE_TYPE` vs raw C `structs`).
     - Adjust opaque type heuristics (e.g., filtering `->priv` makes sense for Linux, but drops critical object models in QEMU).
     - Adjust vtable/class filtering (e.g., aggressively dropping `_ops` works for Linux, but dropping pointer tables destroys context for other C frameworks).

4. **Conduct a Large-Scale Experiment:** 
   Once the micro-scale tests yield fast, dense, and cache-efficient context ingestion, unleash the workflow on a large-scale experiment (500-1000 cases) to validate detection rates and false positive suppression.
