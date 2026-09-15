#!/usr/bin/env bash
# Mint the JWTs that configs/server.yaml's `policy` filter validates.
#
#   ./scripts/issue-token.sh init              generate the signing keypair, once
#   ./scripts/issue-token.sh alice             print a token for "alice", 30 days
#   ./scripts/issue-token.sh alice 7           ...valid 7 days instead
#
# The gateway only ever sees the PUBLIC key, so it can verify tokens but not mint
# them. Revoking one person means reissuing the others and swapping the keypair --
# there is no revocation list here. For real identity management point
# configs/policy.yaml at your IdP's JWKS URL instead and delete this script.
#
# RS256 rather than HS256 with a shared secret: a shared secret would have to live
# inside configs/policy.yaml, which is committed. A public key is not a secret.
set -euo pipefail

cd "$(dirname "$0")/.."

KEY_DIR="${PRAXIS_JWT_DIR:-${XDG_CONFIG_HOME:-${HOME}/.config}/praxis-ai-gateway/jwt}"
PRIVATE_KEY="${KEY_DIR}/private.pem"
PUBLIC_KEY="${KEY_DIR}/public.pem"

# Must match configs/policy.yaml's trusted_issuers entry exactly.
ISSUER="https://praxis-ai-gateway.local"
AUDIENCE="praxis-ai-gateway"

die() { printf '\nerror: %s\n' "$*" >&2; exit 1; }

# base64url, unpadded -- what JWT requires.
b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

init() {
  command -v openssl > /dev/null 2>&1 || die "openssl not found"
  if [ -f "${PRIVATE_KEY}" ]; then
    printf 'keypair already exists: %s\n' "${KEY_DIR}"
    printf 'delete it to rotate -- every existing token stops working.\n'
    return 0
  fi
  mkdir -p "${KEY_DIR}"
  chmod 700 "${KEY_DIR}"
  # Outside the repository on purpose: the private key mints tokens, so it is as
  # sensitive as the provider keys the gateway holds.
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 \
    -out "${PRIVATE_KEY}" 2> /dev/null
  chmod 600 "${PRIVATE_KEY}"
  openssl rsa -in "${PRIVATE_KEY}" -pubout -out "${PUBLIC_KEY}" 2> /dev/null
  chmod 644 "${PUBLIC_KEY}"
  cat <<EOF

keypair created in ${KEY_DIR}
  private.pem  mode 600, mints tokens -- treat like a provider key
  public.pem   mode 644, the gateway verifies with this

Mount the PUBLIC key into the gateway:
  -v ${PUBLIC_KEY}:/etc/praxis/jwt-public.pem:ro

Then issue someone a token:
  $0 alice
EOF
}

mint() {
  local subject="$1" days="${2:-30}" now exp header payload signing_input signature
  [ -f "${PRIVATE_KEY}" ] || die "no keypair yet. Run: $0 init"

  # The subject is interpolated straight into JSON below, so restrict it to
  # characters that need no escaping rather than trying to escape them. A quote
  # or backslash would otherwise produce a token with a malformed payload that
  # only fails later, at the gateway.
  case "${subject}" in
    *[!A-Za-z0-9._@+-]* | "")
      die "subject '${subject}' must be non-empty and use only letters, digits, . _ @ + -"
      ;;
  esac
  # days is used in arithmetic; a non-numeric value would abort with an opaque
  # shell error instead of saying what was wrong.
  case "${days}" in
    '' | *[!0-9]*) die "lifetime '${days}' must be a whole number of days" ;;
  esac
  [ "${days}" -gt 0 ] || die "lifetime must be at least 1 day"

  now="$(date +%s)"
  exp="$(( now + days * 86400 ))"

  header="$(printf '{"alg":"RS256","typ":"JWT"}' | b64url)"
  payload="$(printf '{"iss":"%s","aud":"%s","sub":"%s","iat":%s,"exp":%s}' \
    "${ISSUER}" "${AUDIENCE}" "${subject}" "${now}" "${exp}" | b64url)"
  signing_input="${header}.${payload}"

  signature="$(printf '%s' "${signing_input}" \
    | openssl dgst -sha256 -sign "${PRIVATE_KEY}" | b64url)"

  printf '%s.%s\n' "${signing_input}" "${signature}"
}

case "${1:-}" in
  init) init ;;
  '' | -h | --help | help)
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
    ;;
  *)
    # Pass through, so the default lifetime lives only in mint().
    mint "$@"
    ;;
esac
