#!/bin/sh
# Makes Ghost Run's shared catalog on a Postgres server (#69, decision 0006):
# a `ghost_run` login role and a `ghost_run` database it owns, reachable from
# the LAN with a password. Other databases (DaVinci Resolve's) are left alone.
#
# Run it once, as root, on the database server. For the resolve-db container,
# on the Proxmox host:
#
#   pct push 106 catalog-db-setup.sh /root/catalog-db-setup.sh
#   pct exec 106 -- sh /root/catalog-db-setup.sh [network]
#
# `network` is who may connect (default 192.168.1.0/24, the home LAN). It
# prints what to type into Ghost Run's Settings > Shared catalog. Running it
# again is safe: it gives the role a new password and adds nothing twice.
set -eu

network="${1:-192.168.1.0/24}"
role=ghost_run
db=ghost_run

as_postgres() {
	# $1: SQL for psql -c; output unaligned, without headers.
	su -s /bin/sh postgres -c "psql -X -q -t -A -v ON_ERROR_STOP=1 -c \"$1\""
}

if ! command -v psql >/dev/null 2>&1; then
	echo "psql isn't here: run this on the Postgres server." >&2
	exit 1
fi
version=$(as_postgres "SHOW server_version")
echo "Postgres $version"

password=$(head -c 24 /dev/urandom | base64 | tr -d '/+=' | cut -c 1-24)

# The password goes in through psql's variables on stdin, never on a
# command line (where ps would show it).
su -s /bin/sh postgres -c "psql -X -q -v ON_ERROR_STOP=1 -v role=$role -v db=$db -v pw=$password" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN', :'role')
  WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = :'role') \gexec
ALTER ROLE :"role" WITH LOGIN PASSWORD :'pw';
SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'role')
  WHERE NOT EXISTS (SELECT 1 FROM pg_database WHERE datname = :'db') \gexec
SQL
echo "Role $role and database $db are there."

hba=$(as_postgres "SHOW hba_file")
encryption=$(as_postgres "SHOW password_encryption")
case "$encryption" in
scram-sha-256) method=scram-sha-256 ;;
*) method=md5 ;;
esac
line="host $db $role $network $method"
if grep -qF "$line" "$hba"; then
	echo "$hba already lets $network in."
else
	backup="$hba.before-ghost-run-$(date +%Y%m%d%H%M%S)"
	cp -p "$hba" "$backup"
	# First, so a broader rule further down can't match it first.
	{
		echo "# Ghost Run's shared catalog (added by catalog-db-setup.sh)"
		echo "$line"
		cat "$backup"
	} >"$hba"
	echo "Added to $hba (the old one is $backup):"
	echo "  $line"
fi

listen=$(as_postgres "SHOW listen_addresses")
case "$listen" in
localhost | 127.0.0.1 | "")
	echo "Warning: Postgres only listens on '$listen'. Set listen_addresses = '*' in"
	echo "postgresql.conf and restart it, or other machines can't connect." >&2
	;;
esac
as_postgres "SELECT pg_reload_conf()" >/dev/null

port=$(as_postgres "SHOW port")
address=$(hostname -I 2>/dev/null | awk '{print $1}')
echo
echo "In Ghost Run: Settings > Shared catalog"
echo "  Server:   ${address:-this machine's address}"
echo "  Port:     $port"
echo "  Database: $db"
echo "  User:     $role"
echo "  Password: $password"
echo
echo "Connect on the machine whose catalog you want to keep first (it fills the"
echo "shared catalog); then on the others."
