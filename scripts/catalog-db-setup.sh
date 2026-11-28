#!/bin/sh
# Makes Ghost Run's shared catalog on a Postgres server (#69, decision 0007):
# a `ghost_run` login role and a `ghost_run` database it owns, reachable from
# the LAN with a password. Then lets that role read DaVinci Resolve's project
# databases (decision 0008: the Videos page lists Resolve's projects):
# CONNECT, USAGE and SELECT only, never anything that could change them.
#
# Run it as root, on the database server. For the resolve-db container, on
# the Proxmox host:
#
#   pct push 106 catalog-db-setup.sh /root/catalog-db-setup.sh
#   pct exec 106 -- sh /root/catalog-db-setup.sh [--resolve] [network]
#
# `network` is who may connect (default 192.168.1.0/24, the home LAN). It
# prints what to type into Ghost Run's Settings > Shared catalog. Running it
# again is safe: it gives the role a new password and adds nothing twice.
# With --resolve it does only the Resolve part, for a server set up before
# that was added, and the password stays as it is.
set -eu

resolve_only=
if [ "${1:-}" = --resolve ]; then
	resolve_only=1
	shift
fi
network="${1:-192.168.1.0/24}"
role=ghost_run
db=ghost_run

as_postgres() {
	# $1: SQL for psql -c; output unaligned, without headers.
	su -s /bin/sh postgres -c "psql -X -q -t -A -v ON_ERROR_STOP=1 -c \"$1\""
}

in_db() {
	# SQL on stdin, run in database $1; psql variables role and db are set.
	su -s /bin/sh postgres -c "psql -X -q -t -A -v ON_ERROR_STOP=1 -v role=$role -v db='$1' -d '$1'"
}

if ! command -v psql >/dev/null 2>&1; then
	echo "psql isn't here: run this on the Postgres server." >&2
	exit 1
fi
version=$(as_postgres "SHOW server_version")
echo "Postgres $version"

hba=$(as_postgres "SHOW hba_file")
encryption=$(as_postgres "SHOW password_encryption")
case "$encryption" in
scram-sha-256) method=scram-sha-256 ;;
*) method=md5 ;;
esac

# Puts "host <database $1> ghost_run <network> <method>" first in
# pg_hba.conf, once.
allow() {
	line="host $1 $role $network $method"
	if grep -qF "$line" "$hba"; then
		echo "$hba already lets $network into $1."
		return
	fi
	backup="$hba.before-ghost-run-$(date +%Y%m%d%H%M%S)"
	cp -p "$hba" "$backup"
	# First, so a broader rule further down can't match it first.
	{
		echo "# Ghost Run (added by catalog-db-setup.sh)"
		echo "$line"
		cat "$backup"
	} >"$hba"
	echo "Added to $hba (the old one is $backup):"
	echo "  $line"
}

password=
if [ -z "$resolve_only" ]; then
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
	allow "$db"
elif [ "$(as_postgres "SELECT count(*) FROM pg_roles WHERE rolname = '$role'")" = 0 ]; then
	echo "There's no $role role yet: run this without --resolve first." >&2
	exit 1
fi

# Resolve's project databases: the ones with an SM_Project table.
resolve_dbs=
for name in $(as_postgres "SELECT datname FROM pg_database WHERE NOT datistemplate AND datallowconn AND datname NOT IN ('postgres', '$db')"); do
	found=$(echo "SELECT to_regclass('public.\"SM_Project\"') IS NOT NULL;" | in_db "$name" 2>/dev/null || true)
	if [ "$found" = t ]; then
		resolve_dbs="$resolve_dbs $name"
	fi
done
if [ -z "$resolve_dbs" ]; then
	echo "No Resolve project database here (none has an SM_Project table)."
fi
for name in $resolve_dbs; do
	# Read only, now and for tables Resolve makes later (an upgrade can add
	# some), by whoever owns its tables.
	in_db "$name" >/dev/null <<'SQL'
GRANT CONNECT ON DATABASE :"db" TO :"role";
GRANT USAGE ON SCHEMA public TO :"role";
GRANT SELECT ON ALL TABLES IN SCHEMA public TO :"role";
SELECT format('ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA public GRANT SELECT ON TABLES TO %I',
              tableowner, :'role')
  FROM (SELECT DISTINCT tableowner FROM pg_tables WHERE schemaname = 'public') o \gexec
SQL
	echo "Resolve's database $name: $role may read it."
	allow "$name"
	# What Ghost Run reads (resolve_db::NEEDED), so a Resolve that lays its
	# tables out differently shows up here. Names only, never contents.
	echo "  The columns Ghost Run reads in $name:"
	in_db "$name" <<'SQL'
SELECT '    ' || w.t || '.' || w.c || ': ' ||
       CASE WHEN EXISTS (SELECT 1 FROM information_schema.columns
                         WHERE table_schema = 'public' AND table_name = w.t AND column_name = w.c)
            THEN 'ok' ELSE 'MISSING' END
FROM (VALUES ('SM_Project', 'SM_Project_id'), ('SM_Project', 'ProjectName'),
             ('SM_Project', 'LastModTimeInSecs'), ('Sm2Timeline', 'Sm2Timeline_id'),
             ('Sm2Timeline', 'Name'), ('Sm2Timeline', 'SM_Project_id'),
             ('Sm2Timeline', 'ModTimeInSecs'), ('Sm2Sequence', 'Sm2Sequence_id'),
             ('Sm2Sequence', 'Sm2Timeline_id'), ('Sm2TiTrack', 'Sm2TiTrack_id'),
             ('Sm2TiTrack', 'Type'), ('Sm2TiTrack', 'Sequence'),
             ('Sm2TiItem', 'Sm2TiTrack_id'), ('Sm2TiItem', 'MediaFilePath'),
             ('Sm2TiItem', 'In'), ('Sm2TiItem', 'Duration'), ('Sm2TiItem', 'Start')) AS w(t, c);
SQL
done

listen=$(as_postgres "SHOW listen_addresses")
case "$listen" in
localhost | 127.0.0.1 | "")
	echo "Warning: Postgres only listens on '$listen'. Set listen_addresses = '*' in"
	echo "postgresql.conf and restart it, or other machines can't connect." >&2
	;;
esac
as_postgres "SELECT pg_reload_conf()" >/dev/null

if [ -n "$resolve_only" ]; then
	echo
	echo "Done: Ghost Run's Videos page lists Resolve's projects from here."
	exit 0
fi

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
