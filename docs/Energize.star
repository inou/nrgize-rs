# Energize.star — Elixir/Phoenix deployment config
#
# Usage:
#   nrg run provision          # First-time server setup (Erlang, Elixir, Node, Postgres, Nginx)
#   nrg run deploy             # Full deploy: pull, deps, assets, release, migrate, restart
#   nrg run rollback           # Restart previous release
#   nrg run status             # Check if the app is running
#   nrg run logs               # Tail production logs
#   nrg run console            # Open a remote IEx console
#   nrg run full-setup         # Provision + first deploy (one-shot)
#
# Before running, fill in the placeholders below:
#   - YOUR_SERVER_HOST       → e.g. deploy@198.51.100.10
#   - YOUR_GIT_REPO          → e.g. git@github.com:youruser/yourapp.git
#   - YOUR_APP_NAME          → e.g. my_app (must match mix.exs :app)
#   - YOUR_DOMAIN            → e.g. app.example.com
#   - YOUR_DB_PASSWORD       → the Postgres password for the app user
#   - YOUR_SECRET_KEY_BASE   → generate with: mix phx.gen.secret

# ──────────────────────────────────────────────
# Configuration — replace defaults or override with --var
# ──────────────────────────────────────────────

APP_NAME    = var("app",        default = "YOUR_APP_NAME")       # --var app=my_app
GIT_REPO    = var("repo",       default = "YOUR_GIT_REPO")       # --var repo=git@github.com:user/app.git
DEPLOY_PATH = var("path",       default = "/opt/" + APP_NAME)    # --var path=/var/www/myapp
BRANCH      = var("branch",     default = "main")                # --var branch=develop
DOMAIN      = var("domain",     default = "YOUR_DOMAIN")         # --var domain=app.example.com
DB_NAME     = APP_NAME + "_prod"
DB_USER     = APP_NAME
DB_PASSWORD = var("db_pass",    default = "YOUR_DB_PASSWORD")
SECRET_KEY  = var("secret_key", default = "YOUR_SECRET_KEY_BASE")  # generate with: mix phx.gen.secret
PORT        = var("port",       default = "4000")

# ──────────────────────────────────────────────
# Servers
# ──────────────────────────────────────────────

servers(
    production = "YOUR_SERVER_HOST",
)

# ──────────────────────────────────────────────
# Hooks
# ──────────────────────────────────────────────

before(script = 'echo "▸ Starting at $(date +%H:%M:%S)..."')
success(script = 'echo "✓ All tasks completed successfully."')
error(script = 'echo "✗ Something failed. Check output above."')

# ──────────────────────────────────────────────
# Server provisioning (run once on fresh server)
# ──────────────────────────────────────────────

task(
    name = "provision",
    on = ["production"],
    confirm = "This will install Erlang, Elixir, Node.js, PostgreSQL, and Nginx on the server. Continue?",
    emoji = "🔧",
    script = """
        set -e
        export DEBIAN_FRONTEND=noninteractive

        echo "▸ Updating system packages..."
        sudo apt-get update -qq
        sudo apt-get upgrade -y -qq

        echo "▸ Installing build essentials..."
        sudo apt-get install -y -qq \
            build-essential git curl wget unzip \
            libssl-dev automake autoconf libncurses5-dev

        echo "▸ Installing Erlang & Elixir via ASDF..."
        if [ ! -d "$HOME/.asdf" ]; then
            git clone https://github.com/asdf-vm/asdf.git ~/.asdf --branch v0.14.1
            echo '. "$HOME/.asdf/asdf.sh"' >> ~/.bashrc
        fi
        . "$HOME/.asdf/asdf.sh"

        asdf plugin add erlang 2>/dev/null || true
        asdf plugin add elixir 2>/dev/null || true
        asdf plugin add nodejs 2>/dev/null || true

        echo "▸ Installing Erlang/OTP (this takes a while)..."
        asdf install erlang latest
        asdf global erlang latest

        echo "▸ Installing Elixir..."
        asdf install elixir latest
        asdf global elixir latest

        echo "▸ Installing Node.js..."
        asdf install nodejs latest
        asdf global nodejs latest

        echo "▸ Installing hex and rebar..."
        mix local.hex --force
        mix local.rebar --force

        echo "▸ Installing PostgreSQL..."
        sudo apt-get install -y -qq postgresql postgresql-contrib
        sudo systemctl enable postgresql
        sudo systemctl start postgresql

        echo "▸ Creating database user and database..."
        sudo -u postgres psql -tc "SELECT 1 FROM pg_roles WHERE rolname='""" + DB_USER + """'" | grep -q 1 || \
            sudo -u postgres psql -c "CREATE USER """ + DB_USER + """ WITH PASSWORD '""" + DB_PASSWORD + """';"
        sudo -u postgres psql -tc "SELECT 1 FROM pg_database WHERE datname='""" + DB_NAME + """'" | grep -q 1 || \
            sudo -u postgres psql -c "CREATE DATABASE """ + DB_NAME + """ OWNER """ + DB_USER + """;"

        echo "▸ Installing Nginx..."
        sudo apt-get install -y -qq nginx
        sudo systemctl enable nginx

        echo "▸ Creating deploy directory..."
        sudo mkdir -p """ + DEPLOY_PATH + """
        sudo chown $USER:$USER """ + DEPLOY_PATH + """

        echo "▸ Cloning repository..."
        if [ ! -d """ + DEPLOY_PATH + """/.git ]; then
            git clone """ + GIT_REPO + " " + DEPLOY_PATH + """
        fi

        echo "✓ Server provisioned successfully."
    """,
)

# ──────────────────────────────────────────────
# Nginx config
# ──────────────────────────────────────────────

task(
    name = "setup-nginx",
    on = ["production"],
    emoji = "🌐",
    script = """
        echo "▸ Writing Nginx config..."
        sudo tee /etc/nginx/sites-available/""" + APP_NAME + """ > /dev/null << 'NGINX_CONF'
upstream phoenix {
    server 127.0.0.1:""" + PORT + """;
}

server {
    listen 80;
    server_name """ + DOMAIN + """;

    location / {
        proxy_pass http://phoenix;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location /live {
        proxy_pass http://phoenix;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host $host;
    }
}
NGINX_CONF

        sudo ln -sf /etc/nginx/sites-available/""" + APP_NAME + """ /etc/nginx/sites-enabled/
        sudo rm -f /etc/nginx/sites-enabled/default
        sudo nginx -t
        sudo systemctl reload nginx
        echo "✓ Nginx configured for """ + DOMAIN + """."
    """,
)

# ──────────────────────────────────────────────
# Systemd service
# ──────────────────────────────────────────────

task(
    name = "setup-systemd",
    on = ["production"],
    emoji = "⚙️",
    script = """
        echo "▸ Writing systemd service..."
        sudo tee /etc/systemd/system/""" + APP_NAME + """.service > /dev/null << SYSTEMD_CONF
[Unit]
Description=""" + APP_NAME + """ Phoenix application
After=network.target postgresql.service

[Service]
Type=exec
User=$USER
Group=$USER
WorkingDirectory=""" + DEPLOY_PATH + """
Restart=on-failure
RestartSec=5
SyslogIdentifier=""" + APP_NAME + """

Environment=MIX_ENV=prod
Environment=PHX_SERVER=true
Environment=PORT=""" + PORT + """
Environment=DATABASE_URL=ecto://""" + DB_USER + """:""" + DB_PASSWORD + """@localhost/""" + DB_NAME + """
Environment=SECRET_KEY_BASE=""" + SECRET_KEY + """
Environment=PHX_HOST=""" + DOMAIN + """

ExecStart=""" + DEPLOY_PATH + """/_build/prod/rel/""" + APP_NAME + """/bin/""" + APP_NAME + """ start
ExecStop=""" + DEPLOY_PATH + """/_build/prod/rel/""" + APP_NAME + """/bin/""" + APP_NAME + """ stop

[Install]
WantedBy=multi-user.target
SYSTEMD_CONF

        sudo systemctl daemon-reload
        sudo systemctl enable """ + APP_NAME + """
        echo "✓ Systemd service installed and enabled."
    """,
)

# ──────────────────────────────────────────────
# Environment file
# ──────────────────────────────────────────────

task(
    name = "setup-env",
    on = ["production"],
    emoji = "📝",
    script = """
        echo "▸ Writing .env.prod..."
        cat > """ + DEPLOY_PATH + """/.env.prod << 'ENV_CONF'
export MIX_ENV=prod
export PHX_SERVER=true
export PORT=""" + PORT + """
export DATABASE_URL=ecto://""" + DB_USER + """:""" + DB_PASSWORD + """@localhost/""" + DB_NAME + """
export SECRET_KEY_BASE=""" + SECRET_KEY + """
export PHX_HOST=""" + DOMAIN + """
ENV_CONF

        echo "✓ Environment file written."
    """,
)

# ──────────────────────────────────────────────
# Local build (runs on YOUR machine, not the server)
# ──────────────────────────────────────────────

task(
    name = "build-assets-local",
    local = True,
    emoji = "🔨",
    script = """
        echo "▸ Building assets locally..."
        cd assets && npm install && npm run deploy
        echo "✓ Assets built."
    """,
)

# ──────────────────────────────────────────────
# File upload (push built assets to server)
# ──────────────────────────────────────────────

upload(
    name = "push-assets",
    src = "./priv/static/",
    dest = DEPLOY_PATH + "/priv/static/",
    on = ["production"],
    emoji = "📤",
)

# ──────────────────────────────────────────────
# Deploy tasks
# ──────────────────────────────────────────────

task(
    name = "pull",
    on = ["production"],
    emoji = "📥",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        echo "▸ Pulling branch """ + BRANCH + """..."
        git fetch --all --prune
        git checkout """ + BRANCH + """
        git pull origin """ + BRANCH + """
        echo "✓ Code updated."
    """,
)

task(
    name = "deps",
    on = ["production"],
    emoji = "📦",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        echo "▸ Fetching dependencies..."
        mix deps.get --only prod
        echo "✓ Dependencies installed."
    """,
)

task(
    name = "assets",
    on = ["production"],
    emoji = "🎨",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        echo "▸ Compiling assets..."
        mix assets.deploy
        echo "✓ Assets compiled and digested."
    """,
)

task(
    name = "release",
    on = ["production"],
    emoji = "🏗️",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        echo "▸ Compiling and building release..."
        mix compile
        mix release --overwrite
        echo "✓ Release built."
    """,
)

task(
    name = "migrate",
    on = ["production"],
    emoji = "🗄️",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        echo "▸ Running migrations..."
        """ + DEPLOY_PATH + """/_build/prod/rel/""" + APP_NAME + """/bin/""" + APP_NAME + """ eval '""" + APP_NAME.replace("-", "_") + """.Release.migrate()'
        echo "✓ Migrations complete."
    """,
)

task(
    name = "restart",
    on = ["production"],
    emoji = "🔄",
    script = """
        echo "▸ Restarting """ + APP_NAME + """..."
        sudo systemctl restart """ + APP_NAME + """
        sleep 2
        sudo systemctl is-active --quiet """ + APP_NAME + """ && echo "✓ """ + APP_NAME + """ is running." || echo "✗ """ + APP_NAME + """ failed to start!"
    """,
)

# ──────────────────────────────────────────────
# Utility tasks
# ──────────────────────────────────────────────

task(
    name = "status",
    on = ["production"],
    emoji = "📊",
    script = """
        echo "▸ Service status:"
        sudo systemctl status """ + APP_NAME + """ --no-pager -l || true
        echo ""
        echo "▸ Port """ + PORT + """ listener:"
        ss -tlnp | grep :""" + PORT + """ || echo "(nothing listening)"
    """,
)

task(
    name = "logs",
    on = ["production"],
    emoji = "📜",
    script = """
        sudo journalctl -u """ + APP_NAME + """ -f -n 100
    """,
)

task(
    name = "console",
    on = ["production"],
    emoji = "💻",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        """ + DEPLOY_PATH + """/_build/prod/rel/""" + APP_NAME + """/bin/""" + APP_NAME + """ remote
    """,
)

task(
    name = "rollback",
    on = ["production"],
    confirm = "This will rollback the last migration and restart. Continue?",
    emoji = "⏪",
    script = """
        . "$HOME/.asdf/asdf.sh"
        cd """ + DEPLOY_PATH + """
        source .env.prod
        echo "▸ Rolling back last migration..."
        """ + DEPLOY_PATH + """/_build/prod/rel/""" + APP_NAME + """/bin/""" + APP_NAME + """ eval '""" + APP_NAME.replace("-", "_") + """.Release.rollback(""" + APP_NAME.replace("-", "_") + """.Repo, 1)'
        echo "▸ Restarting..."
        sudo systemctl restart """ + APP_NAME + """
        echo "✓ Rollback complete."
    """,
)

# ──────────────────────────────────────────────
# Macros — composed workflows
# ──────────────────────────────────────────────

# Standard deploy: pull → deps → assets → release → migrate → restart
define_macro(
    name = "deploy",
    tasks = ["pull", "deps", "assets", "release", "migrate", "restart"],
)

# First-time full setup: provision → configure → deploy
define_macro(
    name = "full-setup",
    tasks = ["provision", "setup-nginx", "setup-systemd", "setup-env", "pull", "deps", "assets", "release", "migrate", "restart"],
)

# Build assets locally + push to server + restart
define_macro(
    name = "assets-deploy",
    tasks = ["build-assets-local", "push-assets", "restart"],
)
