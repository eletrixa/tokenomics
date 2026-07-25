# Wiring GCP billing export for a live Gemini spend lane

Why: the `GEMINI_API_KEY` on disk is an AI Studio *inference* key — Google exposes no
billing/spend API for it. The only pull-based programmatic dollar read is the **Cloud Billing
BigQuery export**. This guide wires it; once the export table exists, a live gemini fetcher in
tokenomics becomes a small follow-up spec (BigQuery `jobs.query` lane next to the existing
anthropic/openai lanes).

Verified against Google docs 2026-07-26. Sources at the bottom.

## Steps

1. **Find the GCP project behind the AIza key.**
   Easiest: https://aistudio.google.com/apikey lists each key with its project.
   Scriptable: `GET https://apikeys.googleapis.com/v2/keys:lookupKey?keyString=AIza...` with
   `Authorization: Bearer $(gcloud auth print-access-token)` → `parent` holds the project number.

2. **Check the project is billed at all.**
   `gcloud billing projects describe <PROJECT_ID>` → note `billingAccountName`.
   If `billingEnabled: false` the key is free-tier: spend is definitionally $0, stop here
   (nothing will ever export).

3. **Create the export dataset — multi-region matters.**
   `bq mk --location=US --dataset <PROJECT_ID>:billing_export`
   Must be US or EU multi-region to get the retroactive backfill (start of *previous* month);
   a regional dataset exports only from enable-date forward.

4. **Enable the Standard usage cost export (console only — no gcloud/API exists for this).**
   console.cloud.google.com/billing → pick the billing account → **Billing export** →
   **BigQuery export** tab → *Standard usage cost* → Edit settings → project + dataset from
   step 3 → Save. Needs Billing Account Administrator (or Costs Manager) + BigQuery User.
   Table appears as `gcp_billing_export_v1_<BILLING_ACCT_ID, dashes→underscores>` within hours;
   backfill can take up to 5 days. **Enable this now even if the tokenomics lane comes later —
   history before enablement is unrecoverable.**

5. **Create a headless reader service account.**
   ```bash
   gcloud iam service-accounts create tok-billing-reader --project=<PROJECT_ID>
   gcloud projects add-iam-policy-binding <PROJECT_ID> \
     --member=serviceAccount:tok-billing-reader@<PROJECT_ID>.iam.gserviceaccount.com \
     --role=roles/bigquery.jobUser
   # + roles/bigquery.dataViewer scoped to the billing_export dataset
   gcloud iam service-accounts keys create <keyfile>.json \
     --iam-account=tok-billing-reader@<PROJECT_ID>.iam.gserviceaccount.com
   ```
   Store the key JSON in the secrets vault (`F:/s/keys/`, mirrored to `~/s/keys/`), never in
   the repo. `jobUser` = may run queries; `dataViewer` = may read the export table.

6. **Pin the exact service string once.**
   ```sql
   SELECT DISTINCT service.id, service.description
   FROM `<PROJECT_ID>.billing_export.gcp_billing_export_v1_XXXXXX_XXXXXX_XXXXXX`
   WHERE invoice.month = FORMAT_DATE('%Y%m', CURRENT_DATE())
   ```
   Gemini exports have historically been `service.description = 'Generative Language API'`
   (the API is generativelanguage.googleapis.com); newer surfaces say 'Gemini API'. Pin
   `service.id` — it survives renames.

## The MTD query a daemon runs

```sql
SELECT
  invoice.month AS month,
  ROUND(SUM(cost) + SUM(IFNULL((SELECT SUM(c.amount) FROM UNNEST(credits) c), 0)), 4) AS net_cost,
  ANY_VALUE(currency) AS currency
FROM `<PROJECT_ID>.billing_export.gcp_billing_export_v1_XXXXXX_XXXXXX_XXXXXX`
WHERE invoice.month = FORMAT_DATE('%Y%m', CURRENT_DATE())
  AND service.description IN ('Generative Language API', 'Gemini API')
GROUP BY month
```

The `UNNEST(credits)` term nets out credits — `SUM(cost)` alone overstates vs the invoice.

One synchronous round trip:
`POST https://bigquery.googleapis.com/bigquery/v2/projects/<PROJECT_ID>/queries` with
`{"query": "<sql>", "useLegacySql": false, "timeoutMs": 15000}`, bearer token with scope
`https://www.googleapis.com/auth/bigquery.readonly`. In Rust the `gcp_auth` crate does the
SA-key JWT exchange (point `GOOGLE_APPLICATION_CREDENTIALS` at the key file).

## Lighter alternative (no BigQuery)

Cloud Billing **Budgets + Pub/Sub**: a budget scoped to the project/service pushes JSON
notifications several times a day whose `costAmount` is actual spend-so-far in the period.
Coarser and push-based, but zero BigQuery wiring. Everything else was checked and does not
work: Cloud Billing API has no cost-read endpoint (catalog/links only), `budgets.get` returns
the configured amount not actuals, the AI Studio dashboard and prepaid-credit balance have no
public API, Cloud Monitoring gives request counts not dollars.

## Caveats

- Export lags usage by hours (console cost data up to ~24h); late credits land for days —
  treat MTD as approximate and re-read, never cache as final.
- Free-tier keys: `billingEnabled: false`, nothing exports — detect and skip.
- If the key moved to AI Studio's prepaid-credit billing, that balance has no API; the export
  route covers postpaid Cloud Billing accounts.

## Sources

- https://docs.cloud.google.com/billing/docs/how-to/export-data-bigquery-setup
- https://docs.cloud.google.com/billing/docs/how-to/export-data-bigquery-tables/standard-usage
- https://docs.cloud.google.com/billing/docs/how-to/bq-examples
- https://ai.google.dev/gemini-api/docs/billing
- https://docs.cloud.google.com/api-keys/docs/get-info-api-keys
- https://docs.cloud.google.com/billing/docs/how-to/budget-api-overview
- https://cloud.google.com/bigquery/docs/reference/rest/v2/jobs/query
