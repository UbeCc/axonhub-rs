# Security

axonhub-rs holds upstream API keys. Treat `config.yml` as a secret:

- keep it mode `0600`, owned by the service user;
- prefer `${ENV_VAR}` references and an `EnvironmentFile=` for the service
  manager instead of literals;
- never commit it, paste it into issues, or include it in container images.

The gateway binds `127.0.0.1` by default. If it must listen on a shared
interface, put authentication and TLS in front of it, and set `api_keys` so a
random local process cannot spend your upstream quota.

Report vulnerabilities privately to the repository owner rather than opening a
public issue with exploit details.
