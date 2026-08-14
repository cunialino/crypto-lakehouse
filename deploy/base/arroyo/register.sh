#!/bin/sh
# Registers arroyo connection profiles, connection tables, and SQL pipelines
# from git-tracked declarative files. Run by the arroyo register Sync-hook Job.
# Reads artifacts from /pipelines (ConfigMap arroyo-pipelines):
#   register.sh                     (this script, the Job command)
#   trade_event.proto               (single source of truth, compiled by rustapps)
#   sql/*.sql                       (one file per pipeline)
#   tables/iceberg_trades_sink.json (connection table: sink)
#   tables/nats_trades.json         (connection table: source)
#   tables/trade_gaps_sink.json     (connection table: sink)
#   tables/profiles/*.json          (connection profiles)
#
# Arroyo 0.15 constraints (verified in source):
#   * Connection tables have NO update endpoint and NO per-id GET — only
#     GET/POST/DELETE on the list. A change is delete + recreate.
#   * A table referenced by a pipeline cannot be deleted (FK, no cascade):
#     pipelines must be deleted first. Deleting a pipeline wipes its
#     checkpoints, so any recreate is a full JetStream replay.
#   * GET /connection_tables returns ENRICHED records (compiled_schema,
#     inferred fields with sql_name etc.), so drift is compared against only
#     the keys authored in git (a "skeleton" pick), never the whole object.
#   * Pipelines have no query PATCH; PATCH only sets parallelism/checkpoint/
#     stop. A SQL change is stop -> delete -> recreate.
#
# Two-phase reconcile:
#   Phase 1 (plan): fingerprint git files vs stored records. Profiles first,
#   then tables, then SQL. Nothing is mutated.
#   Phase 2 (apply): if any profile/table drifted, tear down ALL pipelines,
#   then drifted tables/profiles, then rebuild profiles -> tables -> ALL
#   pipelines. If only SQL drifted, recreate just that pipeline.
set -euo pipefail

API_KEY="${ARROYO_API_KEY:?ARROYO_API_KEY not set}"
API="${ARROYO_API:?ARROYO_API not set}"
NATS_URL="${NATS_URL:?NATS_URL not set}"
LAKEKEEPER_URL="${LAKEKEEPER_URL:?LAKEKEEPER_URL not set}"

# Kustomize flattens configMapGenerator files to basenames at the mount root:
# tables/profiles/lakekeeper_catalog.json becomes /pipelines/lakekeeper_catalog.json.
# So all .json live flat in /pipelines; profiles are distinguished from tables by
# the presence of a top-level connection_profile_id key.
JSON_DIR=/pipelines
SQL_DIR=/pipelines
PROTO_FILE=/pipelines/trade_event.proto

apk add --no-cache curl jq >/dev/null 2>&1

AUTH="Authorization: Bearer ${API_KEY}"
CT="Content-Type: application/json"

# ids captured after profile reconcile; tables reference them via {{PROFILE_NATS}}
PROFILE_NATS=""
PROFILE_ICEBERG=""

echo "waiting for arroyo API at ${API}..."
ready=false
for _ in $(seq 1 120); do
  if curl -sf -o /dev/null -H "$AUTH" "${API}/api/v1/connectors" 2>/dev/null; then
    ready=true
    break
  fi
  sleep 5
done
if [ "$ready" != "true" ]; then
  echo "arroyo API not reachable after 10 minutes" >&2
  exit 1
fi
echo "arroyo API ready"

list_profiles() { curl -sf -H "$AUTH" "${API}/api/v1/connection_profiles"; }
list_tables()   { curl -sf -H "$AUTH" "${API}/api/v1/connection_tables"; }
list_pipelines(){ curl -sf -H "$AUTH" "${API}/api/v1/pipelines"; }
pipeline_of()   { curl -sf -H "$AUTH" "${API}/api/v1/pipelines/$1"; }

# Substitute runtime value tokens in a declarative JSON file. Renders to stdout.
subst() {
  local file="$1" proto
  proto=$(cat "$PROTO_FILE")
  jq \
    --arg nats_url "$NATS_URL" --arg lk_url "$LAKEKEEPER_URL" \
    --arg pn "$PROFILE_NATS" --arg pi "$PROFILE_ICEBERG" \
    --arg proto "$proto" \
    'def untoken:
       if type == "string" then
         gsub("\\{\\{NATS_URL\\}\\}";        $nats_url)  |
         gsub("\\{\\{LAKEKEEPER_URL\\}\\}";  $lk_url)    |
         gsub("\\{\\{PROFILE_NATS\\}\\}";    $pn)        |
         gsub("\\{\\{PROFILE_ICEBERG\\}\\}"; $pi)        |
         gsub("\\{\\{PROTO\\}\\}";           $proto)
       else . end;
     walk(untoken)' "$file"
}

# ---------------------------------------------------------------------------
# Drift fingerprints (key-scoped, derived-key-aware).
# ---------------------------------------------------------------------------

# Normalize a record so only git-authored keys are compared. The skeleton is the
# (substituted) git body; pick_skel extracts exactly those keys/paths from the
# stored record, mapping connection_profile_id => stored connection_profile.id,
# and dropping json sinks' bad_data (arroyo serializes it back as null).
pick_skel() { jq -cS --argjson skel "$1" '
  def norm:
    if (.schema.format.type == "json") then del(.schema.bad_data)
    else . end;
  # Each source field is matched to the skeleton element with the same id key
  # (name/field), then compared against ITS OWN skeleton — not element [0] — so
  # per-element shape differences (e.g. a timestamp field carrying "unit", or a
  # config array whose members differ) do not cause false drift.
  def pk($skel; $src):
    if ($skel | type) == "object" then
      reduce ($skel | keys[]) as $k ({};
        if $k == "connection_profile_id" then
          .[$k] = ($src.connection_profile.id // null)
        else
          .[$k] = pk($skel[$k]; $src[$k])
        end)
    elif ($skel | type) == "array" then
      if ($skel | length) == 0 then []
      else
        [ $src | to_entries[] as $e |
          ( ([ $skel[]? | select(($e.value|type)=="object" and
                (if ($e.value|has("name")) then .name == $e.value.name
                 elif ($e.value|has("field")) then .field == $e.value.field
                 else false end) ) ][0]) // $skel[$e.key] // $skel[0]) as $sk |
          pk($sk; $e.value) ]
      end
    else $src
    end;
  norm | pk($skel; .)'; }

# Fingerprint a git profile/table body for drift comparison.
fp_git() { jq -cS '
    if (.schema.format.type == "json") then del(.schema.bad_data)
    else . end'; }

profile_fp() { jq -cS '{name, connector, config}'; }

# ---------------------------------------------------------------------------
# Phase 1: plan (read-only; no mutation).
# ---------------------------------------------------------------------------

PROFILES=""
TABLES=""
PIPELINES=""

# Plan one profile file (read-only). Sets PROFILES_DRIFT=1 if it needs
# create/recreate. The actual create/delete happens in the apply phase, after
# dependent pipelines/tables are torn down (a profile referenced by a table
# cannot be deleted until that table is gone).
plan_profile() {
  local file="$1" body name id stored git
  body=$(subst "$file")
  name=$(printf '%s' "$body" | jq -r '.name')
  id=$(printf '%s' "$PROFILES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
  if [ -z "$id" ]; then
    echo "connection profile $name missing; will create"
    PROFILES_DRIFT=1
  else
    stored=$(printf '%s' "$PROFILES" | jq -c --arg id "$id" '.data[]? | select(.id==$id)' | profile_fp)
    git=$(printf '%s' "$body" | profile_fp)
    if [ "$stored" = "$git" ]; then
      echo "connection profile $name up to date ($id)"
    else
      echo "connection profile $name drifted ($id); will recreate"
      PROFILES_DRIFT=1
    fi
  fi
}

# Apply one profile file (create or delete+recreate the git definition).
apply_profile() {
  local file="$1" body name id stored git
  body=$(subst "$file")
  name=$(printf '%s' "$body" | jq -r '.name')
  id=$(printf '%s' "$PROFILES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
  if [ -z "$id" ]; then
    id=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
      "${API}/api/v1/connection_profiles" | jq -r '.id')
    echo "  created connection profile $name ($id)"
  else
    stored=$(printf '%s' "$PROFILES" | jq -c --arg id "$id" '.data[]? | select(.id==$id)' | profile_fp)
    git=$(printf '%s' "$body" | profile_fp)
    if [ "$stored" != "$git" ]; then
      curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/connection_profiles/$id" >/dev/null || true
      id=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
        "${API}/api/v1/connection_profiles" | jq -r '.id')
      echo "  recreated connection profile $name ($id)"
    fi
  fi
  case "$name" in
    nats_cluster) PROFILE_NATS=$id ;;
    lakekeeper_catalog) PROFILE_ICEBERG=$id ;;
  esac
}

# Plan one table file (read-only). Sets TABLES_DRIFT=1 if it needs
# create/recreate. The actual delete/create happens in the apply phase after
# dependent pipelines are torn down (a table referenced by a pipeline cannot be
# deleted until that pipeline is gone).
plan_table() {
  local file="$1" body name id stored git
  body=$(subst "$file")
  name=$(printf '%s' "$body" | jq -r '.name')
  id=$(printf '%s' "$TABLES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
  if [ -z "$id" ]; then
    echo "connection table $name missing; will create"
    TABLES_DRIFT=1
    return 0
  fi
  stored=$(printf '%s' "$TABLES" | jq -c --arg id "$id" '.data[]? | select(.id==$id)')
  git=$(printf '%s' "$body" | fp_git)
  if [ "$(printf '%s' "$stored" | pick_skel "$git")" = "$git" ]; then
    echo "connection table $name up to date ($id)"
  else
    echo "connection table $name drifted ($id); will recreate"
    TABLES_DRIFT=1
  fi
}

# Apply one table file (create, or delete+recreate when it drifted).
apply_table() {
  local file="$1" body name id stored git
  body=$(subst "$file")
  name=$(printf '%s' "$body" | jq -r '.name')
  id=$(printf '%s' "$TABLES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
  if [ -z "$id" ]; then
    id=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
      "${API}/api/v1/connection_tables" | jq -r '.id')
    echo "  created connection table $name ($id)"
    return 0
  fi
  stored=$(printf '%s' "$TABLES" | jq -c --arg id "$id" '.data[]? | select(.id==$id)')
  git=$(printf '%s' "$body" | fp_git)
  if [ "$(printf '%s' "$stored" | pick_skel "$git")" = "$git" ]; then
    return 0
  fi
  curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/connection_tables/$id" >/dev/null || true
  id=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
    "${API}/api/v1/connection_tables" | jq -r '.id')
  echo "  recreated connection table $name ($id)"
}

# ---------------------------------------------------------------------------
# Phase 2a: teardown (only when profiles/tables drifted).
# ---------------------------------------------------------------------------

wait_terminal() {
  local id="$1" states n_term n_all
  curl -sf -X PATCH -H "$AUTH" -H "$CT" -d '{"stop":"force"}' \
    "${API}/api/v1/pipelines/$id" >/dev/null || true
  echo "  waiting for pipeline jobs to reach a terminal state..."
  for _ in $(seq 1 120); do
    states=$(curl -sf -H "$AUTH" "${API}/api/v1/pipelines/$id/jobs" \
      2>/dev/null | jq -r '[.data[]?.state] | join(" ")' 2>/dev/null || true)
    [ -z "$states" ] && break
    n_term=$(printf '%s\n' $states | grep -cE '^(Stopped|Finished|Failed)$' || true)
    n_all=$(printf '%s\n' $states | grep -c . || true)
    [ "$n_term" -eq "$n_all" ] && break
    sleep 5
  done
  echo "  pipeline job states: ${states:-<none>}"
}

delete_pipeline_by_id() {
  local id="$1" name
  name=$(printf '%s' "$PIPELINES" | jq -r --arg id "$id" '.data[]? | select(.id==$id) | .name' 2>/dev/null || true)
  wait_terminal "$id"
  curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/pipelines/$id" >/dev/null || true
  echo "  deleted pipeline ${name:-$id} ($id)"
}

teardown_all_pipelines() {
  echo "tearing down all pipelines..."
  for id in $(printf '%s' "$PIPELINES" | jq -r '.data[].id'); do
    delete_pipeline_by_id "$id"
  done
}

# ---------------------------------------------------------------------------
# Phase 2b: rebuild.
# ---------------------------------------------------------------------------

validate_sqls() {
  echo "validating SQL pipelines..."
  for f in "$SQL_DIR"/*.sql; do
    [ -e "$f" ] || continue
    body=$(jq -n --arg q "$(sql_query "$f")" '{query:$q, udfs:[]}')
    out=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
      "${API}/api/v1/pipelines/validate_query" 2>/dev/null) || true
    errs=$(printf '%s' "$out" | jq -r '.errors[]?' 2>/dev/null)
    if [ -n "$errs" ]; then
      echo "SQL validation failed for $f:" >&2
      printf '%s\n' "$errs" >&2
      exit 1
    fi
    echo "  ok: $f"
  done
}

# Read the declarative checkpoint interval (micros) from a sql file's leading
# "-- checkpoint_interval_micros: N" comment. Defaults to arroyo's upstream
# default (10s) if the comment is absent or malformed.
pipeline_ci() {
  local v
  v=$(sed -n 's/^-- *checkpoint_interval_micros: *\([0-9][0-9]*\).*/\1/p' "$1" | head -1)
  printf '%s' "${v:-10000000}"
}

# The actual SQL sent to arroyo: file contents minus comment-only lines (the
# declarative header is not part of the query, so it never triggers SQL drift).
sql_query() {
  grep -v '^[[:space:]]*--' "$1"
}

create_pipeline() {
  local name="$1" sqlfile="$2"
  local q body id ci
  q=$(sql_query "$sqlfile")
  ci=$(pipeline_ci "$sqlfile")
  body=$(jq -n --arg name "$name" --arg q "$q" --argjson ci "$ci" \
    '{name:$name, query:$q, udfs:[], parallelism:1, checkpoint_interval_micros:$ci}')
  id=$(curl -sf -X POST -H "$AUTH" -H "$CT" -d "$body" \
    "${API}/api/v1/pipelines" | jq -r '.id')
  echo "created pipeline $name ($id)"
}

# Recreate-or-create a pipeline from git SQL. PATCHes checkpoint interval in
# place (arroyo's PATCH endpoint changes it live, no restart/replay).
upsert_pipeline() {
  local name="$1" sqlfile="$2"
  local id stored q ci stored_ci
  id=$(printf '%s' "$PIPELINES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
  q=$(sql_query "$sqlfile")
  ci=$(pipeline_ci "$sqlfile")
  if [ -z "$id" ]; then
    create_pipeline "$name" "$sqlfile"
    return 0
  fi
  stored=$(pipeline_of "$id" | jq -r '.query' 2>/dev/null || true)
  stored_ci=$(pipeline_of "$id" | jq -r '.checkpoint_interval_micros' 2>/dev/null || true)
  if [ "$stored" != "$q" ]; then
    echo "SQL drift detected for pipeline $name; stopping, deleting, recreating"
    wait_terminal "$id"
    curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/pipelines/$id" >/dev/null || true
    echo "  deleted $name ($id)"
    create_pipeline "$name" "$sqlfile"
  elif [ "$stored_ci" != "$ci" ]; then
    curl -sf -X PATCH -H "$AUTH" -H "$CT" \
      -d "{\"checkpoint_interval_micros\":$ci}" \
      "${API}/api/v1/pipelines/$id" >/dev/null
    echo "updated checkpoint_interval_micros for $name ($ci)"
  else
    echo "pipeline $name up to date"
  fi
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

PROFILES_DRIFT=0
TABLES_DRIFT=0

PROFILES=$(list_profiles)
TABLES=$(list_tables)
PIPELINES=$(list_pipelines)

echo "== plan profiles =="
for f in "$JSON_DIR"/*.json; do
  [ -e "$f" ] || continue
  if ! jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
    plan_profile "$f"
  fi
done

# Resolve {{PROFILE_NATS}}/{{PROFILE_ICEBERG}} to the CURRENTLY stored profile
# ids before planning tables, so tables fingerprint against what actually
# exists, not empty strings. apply_profile() overwrites these with the freshly
# created ids during a rebuild.
PROFILE_NATS=$(printf '%s' "$PROFILES" | jq -r '.data[]? | select(.name=="nats_cluster") | .id' | head -1)
PROFILE_ICEBERG=$(printf '%s' "$PROFILES" | jq -r '.data[]? | select(.name=="lakekeeper_catalog") | .id' | head -1)

echo "== plan tables =="
for f in "$JSON_DIR"/*.json; do
  [ -e "$f" ] || continue
  if jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
    plan_table "$f"
  fi
done

echo "== reconcile =="
if [ "$PROFILES_DRIFT" = "1" ] || [ "$TABLES_DRIFT" = "1" ]; then
  echo "profile/table drift detected; full rebuild required"

  # 1. tear down all pipelines (frees tables; no FK blocks after this).
  teardown_all_pipelines

  # 2. delete drifted tables (tables reference profiles, so tables go first).
  for f in "$JSON_DIR"/*.json; do
    [ -e "$f" ] || continue
    if jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
      name=$(jq -r '.name' "$f")
      id=$(printf '%s' "$TABLES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
      if [ -n "$id" ]; then
        curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/connection_tables/$id" >/dev/null || true
        echo "  deleted table $name ($id)"
      fi
    fi
  done

  # 3. delete drifted profiles.
  for f in "$JSON_DIR"/*.json; do
    [ -e "$f" ] || continue
    if ! jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
      name=$(jq -r '.name' "$f")
      id=$(printf '%s' "$PROFILES" | jq -r --arg n "$name" '.data[]? | select(.name==$n) | .id' | head -1)
      if [ -n "$id" ]; then
        curl -sf -X DELETE -H "$AUTH" "${API}/api/v1/connection_profiles/$id" >/dev/null || true
        echo "  deleted profile $name ($id)"
      fi
    fi
  done

  # 4. rebuild: profiles first (capture new ids), then the tables bound to them.
  echo "== apply profiles =="
  PROFILES=$(list_profiles)
  PROFILE_NATS=""
  PROFILE_ICEBERG=""
  for f in "$JSON_DIR"/*.json; do
    [ -e "$f" ] || continue
    if ! jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
      apply_profile "$f"
    fi
  done

  echo "== apply tables =="
  TABLES=$(list_tables)
  for f in "$JSON_DIR"/*.json; do
    [ -e "$f" ] || continue
    if jq -e 'has("connection_profile_id")' "$f" >/dev/null 2>&1; then
      apply_table "$f"
    fi
  done

  # 5. create all pipelines (none exist anymore — teardown removed them).
  echo "== pipelines (rebuild) =="
  validate_sqls
  PIPELINES="{\"data\":[]}"
  create_pipeline arroyo-lakehouse "$SQL_DIR/lakehouse.sql"
  create_pipeline arroyo-trade-gaps "$SQL_DIR/gaps.sql"
else
  echo "no profile/table drift; reconciling SQL only"
  validate_sqls
  upsert_pipeline arroyo-lakehouse "$SQL_DIR/lakehouse.sql"
  upsert_pipeline arroyo-trade-gaps "$SQL_DIR/gaps.sql"
fi

echo "done"
