# gcc.sashiko.dev

Kubernetes configuration for the Sashiko instance that reviews the GNU
Compiler Collection (GCC). It shares the `sashiko-self` namespace, `pool-256gb`
node, persistent volume (`sashiko-self-data-pvc`), and global static IPv4
address (`lb-ipv4-bugrepo`) with `sashiko.sashiko.dev`:

- It is scheduled on the **same `pool-256gb` node** as `sashiko.sashiko.dev`
  (leaving `n2d-pool` dedicated to `sashiko.dev`) and listens on port **8081**
  rather than `8080`.
- Host-based routing on `sashiko-self-ingress` (`136.110.177.184`) directs
  `sashiko.sashiko.dev` to `sashiko-self-service:8080` and `gcc.sashiko.dev`
  to `sashiko-gcc-service:8081`, attaching both `sashiko-self-cert` and
  `sashiko-gcc-cert`.
- It reviews the **GCC repository** (`git://gcc.gnu.org/git/gcc.git`,
  seeded on first boot from `https://github.com/gcc-mirror/gcc.git`) and
  ingests patches from `gcc-patches@gcc.gnu.org` via NNTP from
  `inbox.sourceware.org` (`inbox.gcc.gcc-patches`).
- Outgoing review emails are muted via `/app/email_policy.toml`
  (`mute_all = true` by default), while SMTP is configured for delivering
  sign-in links.
- Like `sashiko.dev` and `sashiko.sashiko.dev`, it enables **pre-existing bug
  tracking** (`SASHIKO__LINUX_BUG__ENABLED=true`) and periodic upstream fix
  checks (`SASHIKO__LINUX_BUG__FIX_CHECK_ENABLED=true`) against `origin/master`.

## Layout

| Path | Contents |
| --- | --- |
| `base/app/sashiko-gcc-k8s.yaml` | Deployment (`sashiko-gcc-deployment`) on port 8081 |
| `base/app/repo-setup.yaml` | Entrypoint that clones/updates the GCC repository |
| `base/app/management-tools.yaml` | Nightly backup CronJob (`sashiko-gcc-nightly-backup`) |
| `base/routing/` | Service, BackendConfig, ManagedCertificate, shared Ingress, maintenance page |
| `app/`, `routing/`, `maintenance/` | Overlays that are applied |
| `production/kustomization.yaml.example` | Template for the environment specific IDs |

`production/kustomization.yaml` holds the real project IDs and is ignored by
git. Copy the example and edit it before the first apply.

## Deploying

```bash
# 1. Environment specific IDs (first time only).
cp production/kustomization.yaml.example production/kustomization.yaml
$EDITOR production/kustomization.yaml

# 2. Application: Deployment, entrypoint ConfigMap, nightly backups.
kubectl apply -k app/

# 3. Routing: service, health check, certificate, shared Ingress.
kubectl apply -k routing/
```

The managed certificate takes 15 to 60 minutes to go `Active`:

```bash
kubectl get managedcertificate -n sashiko-self -w
```

## Submitting a commit or series manually

To submit a GCC commit by hand against the running pod on port 8081:

```bash
kubectl exec -n sashiko-self deploy/sashiko-gcc-deployment -- \
  curl -s -X POST http://localhost:8081/api/submit \
  -H 'Content-Type: application/json' \
  -d '{"type":"remote","sha":"<commit-sha>","repo":"git://gcc.gnu.org/git/gcc.git"}'
```

## Maintenance page

The Service selector is the switch. Point it at the static page, do the work,
then point it back:

```bash
kubectl apply -k maintenance/   # serve the maintenance page
kubectl apply -k routing/       # back to the application
```

## Data

- Database: `/data/db/sashiko-gcc.db` on `sashiko-self-data-pvc`.
- Repository clone: the `gcc-repo` subpath of the same volume, mounted at
  `/app/third_party/gcc` and refreshed by the entrypoint on every start.
- Worktrees: an `emptyDir` on the node, rebuilt from the clone on demand.
- Backups: nightly at 05:00 America/Los_Angeles into
  `gs://sashiko-data-backup-163135/sashiko-gcc`, staggered an hour after
  `sashiko-self` and two hours after `sashiko.dev`.

To inspect or restore the database, use the shared `data-manager` pod in
`sashiko-self`:

```bash
kubectl exec -it -n sashiko-self deploy/data-manager -- bash
```
