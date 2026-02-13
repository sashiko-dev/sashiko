# NFSD-008: Client state transitions

**Risk**: Protocol violation, resource leak, memory exhaustion

## Check

- Client state transitions follow RFC 8881 specifications
- State progression: INIT→CONFIRMED→ACTIVE→COURTESY→EXPIRED
- Proper handling of client ID confirmation
- Grace period state transitions for reclaim operations
- Client expiry and courtesy state transitions
- COURTESY clients cannot linger indefinitely or accumulate
- State cleanup on client destruction

## Review focus

- Verify state machine logic follows NFS protocol requirements
- Check for race conditions during state transitions
- Ensure proper locking when changing client state
- Validate cleanup paths don't leak resources
- Review courtesy state handling prevents orphaned clients
- Check expiry timers properly bound COURTESY state lifetime

## Example vulnerability

Improper client state transition allowing protocol violation or
COURTESY clients accumulating without expiry causing memory
exhaustion
