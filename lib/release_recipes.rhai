// Optional framework defaults for release::deploy. Caller supplies prepare, activate and
// health. Runtime installation, databases and supervisors remain explicit choices.
private fn merge(defaults, overrides) {
    for key in overrides.keys() {
        if key == "env" {
            for name in overrides.env.keys() { defaults.env[name] = overrides.env[name]; }
        } else { defaults[key] = overrides[key]; }
    }
    defaults
}

fn rails(cfg) {
    merge(#{ env: #{ RAILS_ENV: "production" },
        build: "bundle install && bundle exec rails assets:precompile" }, cfg)
}
fn django(cfg) {
    merge(#{ env: #{}, build: "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt && .venv/bin/python manage.py collectstatic --noinput" }, cfg)
}
fn nextjs(cfg) {
    merge(#{ env: #{ NODE_ENV: "production" }, build: "npm ci --include=dev && npm run build" }, cfg)
}
fn phoenix(cfg) {
    merge(#{ env: #{ MIX_ENV: "prod" },
        build: "mix deps.get --only prod && mix compile && mix assets.deploy && mix release" }, cfg)
}
fn laravel(cfg) {
    merge(#{ env: #{ APP_ENV: "production" },
        build: "composer install --no-dev --no-interaction --prefer-dist --optimize-autoloader && php artisan config:cache && php artisan route:cache && php artisan view:cache" }, cfg)
}
