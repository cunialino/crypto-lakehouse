FROM ghcr.io/dbt-labs/dbt-core:1.11.11

RUN pip install dbt-risingwave

WORKDIR /app/dbt_model

COPY dbt_model/ .

RUN mkdir -p /app/dbt_model/.dbt

COPY <<'EOF' /app/dbt_model/.dbt/profiles.yml
dbt_model:
  outputs:
    prod:
      type: risingwave
      host: risingwave.risingwave.svc.cluster.local
      port: 4567
      dbname: dev
      user: root
      password: ''
      schema: public
      threads: 1
  target: prod
EOF

ENV DBT_PROFILES_DIR=/app/dbt_model/.dbt

WORKDIR /app/dbt_model

ENTRYPOINT ["dbt"]
CMD ["build", "--target", "prod"]
