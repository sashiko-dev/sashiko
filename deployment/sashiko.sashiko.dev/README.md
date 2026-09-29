# sashiko.sashiko.dev

Kubernetes configuration for the Sashiko instance that reviews Sashiko
itself. It is the same image and the same layout as `sashiko.dev`, with three
deliberate differences:

- It runs on the **other node**. The main instance is pinned to `n2d-pool` and
  sizes its requests to fill that node; this one is pinned to `pool-256gb`, so
  a review storm on one instance cannot starve the other and a node drain
  cannot take both down at once.
- It reviews the **Sashiko repository**, not the Linux kernel, and gets its
  work from **GitHub pull request webhooks** rather than from NNTP.
- It has **no SMTP configuration at all**, so it can never mail anyone. The
  sign-in link is written to the pod log instead (see *Signing in* below).
- It leaves **pre-existing bug tracking disabled** (`linux_bug.enabled = false`
  by default, whereas `sashiko.dev` sets `SASHIKO__LINUX_BUG__ENABLED=true`), so
  pre-existing issues are ignored and the Linux bug worker does not run.

## Layout

| Path | Contents |
| --- | --- |
| `base/app/sashiko-self-k8s.yaml` | Namespace, service account, PVC, Deployment |
| `base/app/repo-setup.yaml` | Entrypoint that clones the Sashiko repository |
| `base/app/management-tools.yaml` | Data manager pod and nightly backup CronJob |
| `base/routing/` | Service, BackendConfig, ManagedCertificate, Ingress, maintenance page |
| `app/`, `routing/`, `maintenance/` | Overlays that are applied |
| `production/kustomization.yaml.example` | Template for the environment specific IDs |

`production/kustomization.yaml` holds the real project IDs and is ignored by
git. Copy the example and edit it before the first apply.

## What exists in the cluster already

- Namespace `sashiko-self`.
- Secret `sashiko-self-secrets` with `LLM_API_KEY`, `SASHIKO_SMTP__PASSWORD`,
  `SASHIKO__SERVER__JWT_SECRET` and `SASHIKO__FORGE__WEBHOOK_SECRET`.
  `SASHIKO__FORGE__API_TOKEN` is optional and raises the GitHub API rate limit
  for pull request fetches.
- Workload Identity binding for
  `sashiko-agent-163135.svc.id.goog[sashiko-self/sashiko-self-ksa]` on
  `sashiko-app@sashiko-agent-163135.iam.gserviceaccount.com`, which is what
  lets the backup job write to Cloud Storage.
- DNS: `sashiko.sashiko.dev` already resolves to `136.110.177.184`, the
  reserved address named `lb-ipv4-bugrepo`.

> [!IMPORTANT]
> That address is still claimed by the `bugrepo-ingress` in the `bugrepo`
> namespace, whose Deployment is scaled to zero. A global address backs one
> forwarding rule, so the old Ingress has to go before this one can come up:
> `kubectl delete ingress bugrepo-ingress -n bugrepo`.

## Deploying

```bash
# 1. Environment specific IDs (first time only).
cp production/kustomization.yaml.example production/kustomization.yaml
$EDITOR production/kustomization.yaml

# 2. Release the shared address from the retired bugrepo Ingress.
kubectl delete ingress bugrepo-ingress -n bugrepo

# 3. Application: namespace, service account, PVC, Deployment, backups.
kubectl apply -k app/

# 4. Routing: service, health check, certificate, Ingress.
kubectl apply -k routing/
```

The managed certificate takes 15 to 60 minutes to go `Active`:

```bash
kubectl get managedcertificate -n sashiko-self -w
```

## Signing in

There is no SMTP configuration, so the service prints the sign-in link rather
than mailing it:

```bash
kubectl logs -n sashiko-self deploy/sashiko-self-deployment | grep -i sign-in
```

`SASHIKO__SERVER__ACL__ADMINS` lists who may act on the instance once signed
in.

## Pull requests

The instance ingests pull requests from
`https://github.com/sashiko-dev/sashiko` over the forge webhook:

- Payload URL: `https://sashiko.sashiko.dev/api/webhook/github`
- Content type: `application/json`
- Secret: the value of `SASHIKO__FORGE__WEBHOOK_SECRET` in
  `sashiko-self-secrets`
- Events: *Pull requests*

A request without a valid signature is refused, which is what allows the
endpoint to be exposed at all. To replay a pull request by hand:

```bash
kubectl exec -n sashiko-self deploy/sashiko-self-deployment -- \
  curl -s -X POST http://localhost:8080/api/submit \
  -H 'Content-Type: application/json' \
  -d '{"type":"remote","sha":"<head-sha>","repo":"https://github.com/sashiko-dev/sashiko.git"}'
```

## Maintenance page

The Service selector is the switch. Point it at the static page, do the work,
then point it back:

```bash
kubectl apply -k maintenance/   # serve the maintenance page
kubectl apply -k routing/       # back to the application
```

## Data

- Database: `/data/db/sashiko-self.db` on `sashiko-self-data-pvc` (100Gi).
- Repository clone: the `sashiko-repo` subpath of the same volume, mounted at
  `/app/third_party/sashiko` and refreshed by the entrypoint on every start.
- Worktrees: an `emptyDir` on the node, because they are rebuilt from the
  clone on demand.
- Backups: nightly at 04:00 America/Los_Angeles into
  `gs://sashiko-data-backup-163135/sashiko-self`, an hour after the main
  instance so the two never contend.

To inspect or restore the database, use the data manager pod, which mounts the
same volume without holding the database open:

```bash
kubectl exec -it -n sashiko-self deploy/data-manager -- bash
```
