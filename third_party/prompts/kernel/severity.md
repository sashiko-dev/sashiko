# Kernel Severity Assessment: The Escalation Protocol

You must classify every identified regression using this gated protocol. Your goal is to maintain a professional distribution of findings: the vast majority (>75%) should be Low, and <1% should be Critical.

### THE RULE OF DEFAULT LOW
Every bug, logic error, or violation is **Low Severity** by default. You may only escalate if you can provide technical proof that it meets the specific gates below. If you are unsure or cannot prove a higher impact, it **MUST** remain Low.

---

### STEP 1: Path Classification
Before assigning severity, identify the "hotness" of the code path:
- **HOT**: Per-packet, per-syscall, scheduler loop, interrupt context, high-frequency locks, or any code that runs millions of times.
- **COLD**: Rare error paths, ioctl setup, slow-path configuration, or code that runs only on specific user triggers.
- **INIT**: Boot-time only, module load/unload, one-time hardware probe, or setup code that runs exactly once.

### STEP 2: The Escalation Gates

#### GATE 1: Escalate to MEDIUM?
**Criteria (Must meet ONE):**
- **Functional Deviation**: The code does something objectively wrong (e.g., returns the wrong error code) and it has a measurable impact on system behavior, even if small.
- **Maintainability**: The change makes future maintenance significantly riskier (e.g., severe anti-patterns or extremely confusing logic).
- **Silent Failure**: An error occurs but is swallowed, making debugging significantly harder.

#### GATE 2: Escalate to HIGH?
**Criteria (Must meet ONE):**
- **System Stability**: The bug causes a kernel Oops/Panic, but it requires `root` privileges or very specific/rare hardware state to trigger.
- **Functional Loss**: A core feature (e.g., a specific filesystem mount, a networking protocol, or a driver) stops working entirely.
- **Resource Leak**: A memory or lock leak in a **HOT** or **COLD** path (but NOT an **INIT** path).
- **Race Condition**: A demonstrable race that can cause data corruption or deadlocks under load.

#### GATE 3: Escalate to CRITICAL?
**Criteria (Must meet ONE):**
- **Unprivileged/Remote Crash**: Any user on the system (or network) can crash the kernel without special permissions.
- **Security Breach**: Local Privilege Escalation (LPE) or Remote Code Execution (RCE).
- **Silent Data Corruption**: The system silently writes the wrong data to disk or memory (e.g., a bug in the block layer, filesystem, or page cache).
- **Memory Corruption**: Use-After-Free, Double-Free, or OOB-Write in a **HOT** path.

---

### STEP 3: Mandatory Justification
For every finding, you must include a "Severity Justification" section in your internal notes before generating the report:

1. **Path**: [Hot/Cold/Init]
2. **Trigger**: [Remote / Unprivileged Local / Root-only / Rare Hardware]
3. **Escalation Proof**: "This is not [Level Below] because..."
   - *Example: "This is not Medium because the leak happens in the per-packet path (HOT), meaning it will OOM the server in minutes, which constitutes a High stability issue."*

### Final Check: The "So What?" Test
- If it's a bug but the system keeps running fine and most users won't notice: **LOW**.
- If it's a bug that requires a developer to spend 2 days debugging a weird edge case: **MEDIUM**.
- If it's a bug that makes a server stop serving traffic or requires a reboot: **HIGH**.
- If it's a bug that gets you a CVE or loses user data: **CRITICAL**.
