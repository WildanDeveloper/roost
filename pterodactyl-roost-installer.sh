#!/bin/bash

set -e

######################################################################################
#                                                                                    #
# Project 'pterodactyl-roost-installer'                                              #
#                                                                                    #
# A combined Pterodactyl Panel + Roost installer, modeled after the community        #
# 'pterodactyl-installer' script by Vilhelm Prytz (GPL-3.0):                         #
#   https://github.com/pterodactyl-installer/pterodactyl-installer                   #
#                                                                                    #
# Differences from the original:                                                     #
#   * installs Roost (Wings-compatible daemon written in Rust) instead of Wings      #
#   * option 0 configures the daemon automatically: the location, the node and       #
#     /etc/pterodactyl/config.yml are created from the panel itself via              #
#     `php artisan p:node:configuration` — no auto-deploy token, no manual copy      #
#                                                                                    #
# This script is not associated with the official Pterodactyl Project.               #
#                                                                                    #
######################################################################################

LOG_PATH="/var/log/pterodactyl-roost-installer.log"

# ------------------ Variables ----------------- #

# download URLs
export PANEL_DL_URL="https://github.com/pterodactyl/panel/releases/latest/download/panel.tar.gz"
export ROOST_DL_BASE_URL="https://github.com/${ROOST_REPO:-WildanDeveloper/roost}/releases/latest/download/roost_linux_"
# Optional local roost binary (skip download)
export ROOST_BINARY="${ROOST_BINARY:-}"

# Colors
COLOR_YELLOW='\033[1;33m'
COLOR_GREEN='\033[0;32m'
COLOR_RED='\033[0;31m'
COLOR_CYAN='\033[0;36m'
COLOR_NC='\033[0m'

# email validation regex
email_regex="^(([A-Za-z0-9]+((\.|\-|\_|\+)?[A-Za-z0-9]?)*[A-Za-z0-9]+)|[A-Za-z0-9]+)@(([A-Za-z0-9]+)+((\.|\-|\_)?([A-Za-z0-9]+)+)*)+\.([A-Za-z]{2,})+$"

# Charset used to generate random passwords
password_charset='A-Za-z0-9!"#%&()*+,-./:;<=>?@[\]^_`{|}~'

# --------------------- Lib -------------------- #

lib_loaded() {
  return 0
}

output() {
  echo -e "* $1"
}

success() {
  echo ""
  output "${COLOR_GREEN}SUCCESS${COLOR_NC}: $1"
  echo ""
}

error() {
  echo ""
  echo -e "* ${COLOR_RED}ERROR${COLOR_NC}: $1" 1>&2
  echo ""
}

warning() {
  echo ""
  output "${COLOR_YELLOW}WARNING${COLOR_NC}: $1"
  echo ""
}

print_brake() {
  for ((n = 0; n < $1; n++)); do
    echo -n "#"
  done
  echo ""
}

hyperlink() {
  echo -e "\e]8;;${1}\a${1}\e]8;;\a"
}

get_latest_release() {
  curl -sL "https://api.github.com/repos/$1/releases/latest" |
    grep '"tag_name":' |
    sed -E 's/.*"([^"]+)".*/\1/'
}

# First argument is panel / roost / neither
welcome() {
  print_brake 70
  output "Pterodactyl panel + roost installation script"
  output ""
  output "Based on the community pterodactyl-installer (GPL-3.0) by Vilhelm Prytz"
  output "https://github.com/pterodactyl-installer/pterodactyl-installer"
  output ""
  output "This script is not associated with the official Pterodactyl Project."
  output ""
  output "Running $OS version $OS_VER."
  print_brake 70
}

array_contains_element() {
  local e match="$1"
  shift
  for e; do [[ "$e" == "$match" ]] && return 0; done
  return 1
}

valid_email() {
  [[ $1 =~ ${email_regex} ]]
}

invalid_ip() {
  ip route get "$1" >/dev/null 2>&1
  echo $?
}

gen_passwd() {
  local length=$1
  local password=""
  while [ ${#password} -lt "$length" ]; do
    password=$(echo "$password""$(head -c 100 /dev/urandom | LC_ALL=C tr -dc "$password_charset")" | fold -w "$length" | head -n 1)
  done
  echo "$password"
}

# --------------- User input functions ---------- #

required_input() {
  local __resultvar=$1
  local result=''

  while [ -z "$result" ]; do
    echo -n "* ${2}"
    read -r result

    if [ -z "${3}" ]; then
      [ -z "$result" ] && result="${4}"
    else
      [ -z "$result" ] && error "${3}"
    fi
  done

  eval "$__resultvar="'$result'""
}

email_input() {
  local __resultvar=$1
  local result=''

  while ! valid_email "$result"; do
    echo -n "* ${2}"
    read -r result

    valid_email "$result" || error "${3}"
  done

  eval "$__resultvar="'$result'""
}

password_input() {
  local __resultvar=$1
  local result=''
  local default="$4"

  while [ -z "$result" ]; do
    echo -n "* ${2}"

    while IFS= read -r -s -n1 char; do
      [[ -z $char ]] && {
        printf '\n'
        break
      }
      if [[ $char == $'\x7f' ]]; then
        if [ -n "$result" ]; then
          [[ -n $result ]] && result=${result%?}
          printf '\b \b'
        fi
      else
        result+=$char
        printf '*'
      fi
    done
    [ -z "$result" ] && [ -n "$default" ] && result="$default"
    [ -z "$result" ] && error "${3}"
  done

  eval "$__resultvar="'$result'""
}

ask_firewall() {
  local __resultvar=$1
  echo -e -n "* Do you want to automatically configure UFW (firewall)? (y/N): "
  read -r CONFIRM_UFW
  if [[ "$CONFIRM_UFW" =~ [Yy] ]]; then
    eval "$__resultvar="'true'""
  fi
}

install_firewall() {
  output ""
  output "Installing Uncomplicated Firewall (UFW)"
  if ! [ -x "$(command -v ufw)" ]; then
    update_repos true
    install_packages "ufw" true
  fi
  ufw --force enable
  success "Enabled Uncomplicated Firewall (UFW)"
}

firewall_allow_ports() {
  for port in $1; do
    ufw allow "$port"
  done
  ufw --force reload
}

verify_fqdn() {
  local fqdn="$1"
  local dns_server="8.8.8.8"
  local ip dns_record

  output "Resolving DNS for $fqdn"
  ip=$(curl -4 -fs --max-time 10 https://api.ipify.org || hostname -I | awk '{print $1}')
  dns_record=$(dig +short @$dns_server "$fqdn" | tail -n1)

  if [ "${ip}" != "${dns_record}" ]; then
    output "The DNS record ($dns_record) does not match your server IP. Please make sure the FQDN $fqdn is pointing to the IP of your server, $ip"
    output "If you are using Cloudflare, please disable the proxy or opt out from Let's Encrypt."
    echo -n "* Proceed anyways (your install will be broken if you do not know what you are doing)? (y/N): "
    read -r override
    [[ "$override" =~ [Yy] ]] || { error "Invalid FQDN or DNS record"; exit 1; }
  else
    output "DNS verified!"
  fi
}

# -------------------- MYSQL ------------------- #

create_db_user() {
  local db_user_name="$1"
  local db_user_password="$2"
  local db_host="${3:-127.0.0.1}"

  output "Creating database user $db_user_name..."
  mariadb -u root -e "CREATE USER '$db_user_name'@'$db_host' IDENTIFIED BY '$db_user_password';"
  mariadb -u root -e "FLUSH PRIVILEGES;"
  output "Database user $db_user_name created"
}

grant_all_privileges() {
  local db_name="$1"
  local db_user_name="$2"
  local db_host="${3:-127.0.0.1}"

  output "Granting all privileges on $db_name to $db_user_name..."
  mariadb -u root -e "GRANT ALL PRIVILEGES ON $db_name.* TO '$db_user_name'@'$db_host' WITH GRANT OPTION;"
  mariadb -u root -e "FLUSH PRIVILEGES;"
  output "Privileges granted"
}

create_db() {
  local db_name="$1"
  local db_user_name="$2"
  local db_host="${3:-127.0.0.1}"

  output "Creating database $db_name..."
  mariadb -u root -e "CREATE DATABASE $db_name;"
  grant_all_privileges "$db_name" "$db_user_name" "$db_host"
  output "Database $db_name created"
}

# --------------- Package Manager -------------- #

update_repos() {
  local args=""
  [[ "$1" == true ]] && args="-qq"
  output "Updating package repositories..."
  apt-get update -y $args
}

install_packages() {
  local args=""
  [[ $2 == true ]] && args="-qq"
  eval apt-get -y $args install "$1"
}

# ---------------- System checks --------------- #

check_os() {
  export DEBIAN_FRONTEND=noninteractive
  case "$OS" in
  ubuntu)
    [ "$OS_VER_MAJOR" == "22" ] && SUPPORTED=true
    [ "$OS_VER_MAJOR" == "24" ] && SUPPORTED=true
    [ "$OS_VER_MAJOR" == "26" ] && SUPPORTED=true
    ;;
  debian)
    [ "$OS_VER_MAJOR" == "12" ] && SUPPORTED=true
    [ "$OS_VER_MAJOR" == "13" ] && SUPPORTED=true
    ;;
  *)
    SUPPORTED=false
    ;;
  esac

  if [ "$SUPPORTED" == false ]; then
    output "$OS $OS_VER is not supported"
    error "Unsupported OS (Debian 12/13 and Ubuntu 22.04/24.04+ only)"
    exit 1
  fi
}

check_virt() {
  output "Installing virt-what..."
  update_repos true
  install_packages "virt-what" true
  export PATH="$PATH:/sbin:/usr/sbin"

  virt_serv=$(virt-what || true)
  case "$virt_serv" in
  *openvz* | *lxc*)
    warning "Unsupported type of virtualization detected. Please consult with your hosting provider whether your server can run Docker or not. Proceed at your own risk."
    echo -e -n "* Are you sure you want to proceed? (y/N): "
    read -r CONFIRM_PROCEED
    if [[ ! "$CONFIRM_PROCEED" =~ [Yy] ]]; then
      error "Installation aborted!"
      exit 1
    fi
    ;;
  *)
    [ "$virt_serv" != "" ] && warning "Virtualization: $virt_serv detected."
    ;;
  esac

  success "System is compatible with docker"
}

# ================== PANEL ===================== #

# --------------- Panel variables -------------- #

FQDN="${FQDN:-}"
MYSQL_DB="${MYSQL_DB:-panel}"
MYSQL_USER="${MYSQL_USER:-pterodactyl}"
MYSQL_PASSWORD="${MYSQL_PASSWORD:-}"
timezone="${timezone:-Asia/Jakarta}"
telemetry="${telemetry:-}"
ASSUME_SSL="${ASSUME_SSL:-false}"
CONFIGURE_LETSENCRYPT="${CONFIGURE_LETSENCRYPT:-false}"
CONFIGURE_FIREWALL="${CONFIGURE_FIREWALL:-false}"
SSL_AVAILABLE=false
PANEL_DIR="/var/www/pterodactyl"

# panel answers (collect_panel_answers or env in headless mode)
email="${email:-}"
user_email="${user_email:-}"
user_username="${user_username:-}"
user_firstname="${user_firstname:-}"
user_lastname="${user_lastname:-}"
user_password="${user_password:-}"

# -------- Panel user input functions ----------- #

ask_letsencrypt() {
  if [ "$CONFIGURE_FIREWALL" == false ]; then
    warning "Let's Encrypt requires port 80/443 to be opened! You have opted out of the automatic firewall configuration; use this at your own risk (if port 80/443 is closed, the script will fail)!"
  fi

  echo -e -n "* Do you want to automatically configure HTTPS using Let's Encrypt? (y/N): "
  read -r CONFIRM_SSL

  if [[ "$CONFIRM_SSL" =~ [Yy] ]]; then
    CONFIGURE_LETSENCRYPT=true
    ASSUME_SSL=false
  fi
}

ask_assume_ssl() {
  output "Let's Encrypt is not going to be automatically configured by this script (user opted out)."
  output "You can 'assume' Let's Encrypt, which means the script will download a nginx configuration that is configured to use a Let's Encrypt certificate but the script won't obtain the certificate for you."
  output "If you assume SSL and do not obtain the certificate, your installation will not work."
  echo -n "* Assume SSL or not? (y/N): "
  read -r ASSUME_SSL_INPUT

  [[ "$ASSUME_SSL_INPUT" =~ [Yy] ]] && ASSUME_SSL=true
  true
}

ask_telemetry() {
  output "Pterodactyl Panel collects anonymous telemetry data to help steer the development."
  output "More Info: https://pterodactyl.io/panel/1.0/additional_configuration.html#telemetry"
  echo -n "* Enable sending anonymous telemetry data? (yes/no) [yes]: "
  read -r telemetry_input

  if [[ -z "$telemetry_input" ]] || [[ "$telemetry_input" =~ ^([Yy]|[Yy]es)$ ]]; then
    telemetry="true"
  else
    telemetry="false"
  fi
}

check_FQDN_SSL() {
  if [[ $(invalid_ip "$FQDN") == 1 && $FQDN != 'localhost' ]]; then
    SSL_AVAILABLE=true
  else
    warning "* Let's Encrypt will not be available for IP addresses."
    output "To use Let's Encrypt, you must use a valid domain name."
  fi
}

panel_summary() {
  print_brake 62
  output "Pterodactyl panel with nginx on $OS"
  output "Database name: $MYSQL_DB"
  output "Database user: $MYSQL_USER"
  output "Database password: (censored)"
  output "Timezone: $timezone"
  output "Email: $email"
  output "User email: $user_email"
  output "Username: $user_username"
  output "First name: $user_firstname"
  output "Last name: $user_lastname"
  output "User password: (censored)"
  output "Hostname/FQDN: $FQDN"
  output "Configure Firewall? $CONFIGURE_FIREWALL"
  output "Configure Let's Encrypt? $CONFIGURE_LETSENCRYPT"
  output "Assume SSL? $ASSUME_SSL"
  output "Telemetry: $telemetry"
  print_brake 62
}

# ------------ Panel installation -------------- #

enable_services() {
  systemctl enable redis-server >/dev/null 2>&1 || systemctl enable redis >/dev/null 2>&1 || true
  systemctl start redis-server >/dev/null 2>&1 || systemctl start redis >/dev/null 2>&1 || true
  systemctl enable nginx >/dev/null 2>&1 || true
  systemctl enable mariadb >/dev/null 2>&1 || true
  systemctl start mariadb >/dev/null 2>&1 || true
}

dep_install() {
  output "Installing dependencies for $OS $OS_VER..."

  [ "$CONFIGURE_FIREWALL" == true ] && install_firewall && firewall_ports

  update_repos true

  # Add repos for PHP 8.3 (panel requires ^8.2 || ^8.3; Debian 13/Ubuntu ship
  # newer or older defaults, so the sury repo pins a known-good version).
  install_packages "dirmngr ca-certificates apt-transport-https lsb-release gnupg" true
  [ "$OS" == "ubuntu" ] && { install_packages "software-properties-common" true; add-apt-repository -y universe >/dev/null 2>&1 || true; }

  curl -fsSL https://packages.sury.org/php/apt.gpg | gpg --dearmor --yes -o /etc/apt/trusted.gpg.d/php.gpg
  echo "deb https://packages.sury.org/php/ $(lsb_release -sc) main" | tee /etc/apt/sources.list.d/php.list >/dev/null

  update_repos true

  install_packages "php8.3 php8.3-{cli,common,gd,mysql,mbstring,bcmath,xml,fpm,curl,zip,intl,redis} \
    mariadb-common mariadb-server mariadb-client \
    nginx \
    redis-server \
    zip unzip tar \
    git cron dnsutils"

  [ "$CONFIGURE_LETSENCRYPT" == true ] && install_packages "certbot python3-certbot-nginx"

  enable_services

  success "Dependencies installed!"
}

firewall_ports() {
  output "Opening ports: 22 (SSH), 80 (HTTP) and 443 (HTTPS)"
  firewall_allow_ports "22 80 443"
  success "Firewall ports opened!"
}

install_composer() {
  output "Installing composer.."
  curl -sS https://getcomposer.org/installer | php -- --install-dir=/usr/local/bin --filename=composer
  success "Composer installed!"
}

ptdl_dl() {
  output "Downloading pterodactyl panel files .. "
  mkdir -p "$PANEL_DIR"
  cd "$PANEL_DIR"

  curl -fsSLo panel.tar.gz "${PTERO_PANEL_TARBALL:-$PANEL_DL_URL}"
  tar -xzvf panel.tar.gz >/dev/null
  chmod -R 755 storage/* bootstrap/cache/

  cp .env.example .env

  success "Downloaded pterodactyl panel files!"
}

install_composer_deps() {
  output "Installing composer dependencies.."
  COMPOSER_ALLOW_SUPERUSER=1 composer install --no-dev --optimize-autoloader -q
  success "Installed composer dependencies!"
}

configure() {
  output "Configuring environment.."

  local app_url="http://$FQDN"
  [ "$ASSUME_SSL" == true ] && app_url="https://$FQDN"
  [ "$CONFIGURE_LETSENCRYPT" == true ] && app_url="https://$FQDN"

  php artisan key:generate --force

  php artisan p:environment:setup \
    --author="$email" \
    --url="$app_url" \
    --timezone="$timezone" \
    --cache="redis" \
    --session="redis" \
    --queue="redis" \
    --redis-host="127.0.0.1" \
    --redis-pass="null" \
    --redis-port="6379" \
    --telemetry="$telemetry" \
    --settings-ui=true

  php artisan p:environment:database \
    --host="127.0.0.1" \
    --port="3306" \
    --database="$MYSQL_DB" \
    --username="$MYSQL_USER" \
    --password="$MYSQL_PASSWORD"

  output "Running database migrations (this may take a minute)..."
  php artisan migrate --seed --force

  php artisan p:user:make \
    --email="$user_email" \
    --username="$user_username" \
    --name-first="$user_firstname" \
    --name-last="$user_lastname" \
    --password="$user_password" \
    --admin=1

  success "Configured environment!"
}

set_folder_permissions() {
  chown -R www-data:www-data "$PANEL_DIR" || true
}

insert_cronjob() {
  output "Installing cronjob.. "
  crontab -l 2>/dev/null | {
    cat
    echo "* * * * * php /var/www/pterodactyl/artisan schedule:run >> /dev/null 2>&1"
  } | crontab -

  success "Cronjob installed!"
}

install_pteroq() {
  output "Installing pteroq service.."

  cat > /etc/systemd/system/pteroq.service <<'EOF'
[Unit]
Description=Pterodactyl Queue Worker
After=redis-server.service

[Service]
User=www-data
Group=www-data
Restart=always
ExecStart=/usr/bin/php /var/www/pterodactyl/artisan queue:work --queue=high,standard,low --sleep=3 --tries=3
StartLimitInterval=180
StartLimitBurst=30
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF

  systemctl daemon-reload
  systemctl enable pteroq.service >/dev/null 2>&1
  systemctl start pteroq >/dev/null 2>&1 || true

  success "Installed pteroq!"
}

configure_nginx() {
  output "Configuring nginx .."

  local PHP_SOCKET="/run/php/php8.3-fpm.sock"
  local CONFIG_PATH_AVAIL="/etc/nginx/sites-available"
  local CONFIG_PATH_ENABL="/etc/nginx/sites-enabled"

  rm -rf "$CONFIG_PATH_ENABL"/default

  if [ "$ASSUME_SSL" == true ]; then
    cat > "$CONFIG_PATH_AVAIL"/pterodactyl.conf <<'EOF'
server_tokens off;

server {
    listen 80;
    listen [::]:80;

    server_name <domain>;
    return 301 https://$server_name$request_uri;
}

server {
    listen 443 ssl http2;
    listen [::]:443 ssl http2;

    server_name <domain>;

    root /var/www/pterodactyl/public;
    index index.php;

    access_log /var/log/nginx/pterodactyl.app-access.log;
    error_log  /var/log/nginx/pterodactyl.app-error.log error;

    client_max_body_size 100m;
    client_body_timeout 120s;

    sendfile off;

    ssl_certificate /etc/ssl/<domain>.pem;
    ssl_certificate_key /etc/ssl/<domain>.key;
    ssl_session_cache shared:SSL:10m;
    ssl_protocols TLSv1.2 TLSv1.3;
    ssl_ciphers "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384";
    ssl_prefer_server_ciphers on;

    add_header X-Content-Type-Options nosniff;
    add_header X-XSS-Protection "1; mode=block";
    add_header X-Robots-Tag none;
    add_header Content-Security-Policy "frame-ancestors 'self'";
    add_header X-Frame-Options DENY;
    add_header Referrer-Policy same-origin;

    location / {
        try_files $uri $uri/ /index.php?$query_string;
    }

    location ~ \.php$ {
        fastcgi_split_path_info ^(.+\.php)(/.+)$;
        fastcgi_pass unix:<php_socket>;
        fastcgi_index index.php;
        include fastcgi_params;
        fastcgi_param PHP_VALUE "upload_max_filesize = 100M \n post_max_size=100M";
        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
        fastcgi_param HTTP_PROXY "";
        fastcgi_intercept_errors off;
        fastcgi_buffer_size 16k;
        fastcgi_buffers 4 16k;
        fastcgi_connect_timeout 300;
        fastcgi_send_timeout 300;
        fastcgi_read_timeout 300;
    }

    location ~ /\.ht {
        deny all;
    }
}
EOF
  else
    cat > "$CONFIG_PATH_AVAIL"/pterodactyl.conf <<'EOF'
server {
    listen 80;
    listen [::]:80;

    server_name <domain>;

    root /var/www/pterodactyl/public;
    index index.html index.htm index.php;
    charset utf-8;

    location / {
        try_files $uri $uri/ /index.php?$query_string;
    }

    location = /favicon.ico { access_log off; log_not_found off; }
    location = /robots.txt  { access_log off; log_not_found off; }

    access_log off;
    error_log  /var/log/nginx/pterodactyl.app-error.log error;

    client_max_body_size 100m;
    client_body_timeout 120s;

    sendfile off;

    location ~ \.php$ {
        fastcgi_split_path_info ^(.+\.php)(/.+)$;
        fastcgi_pass unix:<php_socket>;
        fastcgi_index index.php;
        include fastcgi_params;
        fastcgi_param PHP_VALUE "upload_max_filesize = 100M \n post_max_size=100M";
        fastcgi_param SCRIPT_FILENAME $document_root$fastcgi_script_name;
        fastcgi_param HTTP_PROXY "";
        fastcgi_intercept_errors off;
        fastcgi_buffer_size 16k;
        fastcgi_buffers 4 16k;
        fastcgi_connect_timeout 300;
        fastcgi_send_timeout 300;
        fastcgi_read_timeout 300;
    }

    location ~ /\.ht {
        deny all;
    }
}
EOF
  fi

  sed -i -e "s@<domain>@${FQDN}@g" "$CONFIG_PATH_AVAIL"/pterodactyl.conf
  sed -i -e "s@<php_socket>@${PHP_SOCKET}@g" "$CONFIG_PATH_AVAIL"/pterodactyl.conf

  ln -sf "$CONFIG_PATH_AVAIL"/pterodactyl.conf "$CONFIG_PATH_ENABL"/pterodactyl.conf

  nginx -t >/dev/null 2>&1 || { error "nginx configuration is invalid:"; nginx -t; exit 1; }
  systemctl restart nginx

  success "Nginx configured!"
}

letsencrypt() {
  FAILED=false

  output "Configuring Let's Encrypt..."

  certbot --nginx --redirect --no-eff-email --email "$email" -d "$FQDN" || FAILED=true

  if [ ! -d "/etc/letsencrypt/live/$FQDN/" ] || [ "$FAILED" == true ]; then
    warning "The process of obtaining a Let's Encrypt certificate failed!"
    echo -n "* Still assume SSL? (y/N): "
    read -r CONFIGURE_SSL

    if [[ "$CONFIGURE_SSL" =~ [Yy] ]]; then
      ASSUME_SSL=true
      CONFIGURE_LETSENCRYPT=false
      configure_nginx
    else
      ASSUME_SSL=false
      CONFIGURE_LETSENCRYPT=false
    fi
  else
    success "The process of obtaining a Let's Encrypt certificate succeeded!"
  fi
}

perform_install_panel() {
  output "Starting installation.. this might take a while!"
  dep_install
  install_composer
  ptdl_dl
  install_composer_deps
  create_db_user "$MYSQL_USER" "$MYSQL_PASSWORD"
  create_db "$MYSQL_DB" "$MYSQL_USER"
  configure
  set_folder_permissions
  insert_cronjob
  install_pteroq
  configure_nginx
  [ "$CONFIGURE_LETSENCRYPT" == true ] && letsencrypt

  return 0
}

# ================== ROOST ===================== #

ROOST_BIN="/usr/local/bin/roost"
ROOST_CONFIG_PATH="/etc/pterodactyl/config.yml"
NODE_NAME="${NODE_NAME:-Node-1}"
LOC_SHORT="${LOC_SHORT:-main}"
LOC_LONG="${LOC_LONG:-Primary location}"

# Auto-config results (filled by the node bootstrap step)
NODE_ID=""
NODE_FQDN="${NODE_FQDN:-}"
NODE_SCHEME="http"
NODE_LETSENCRYPT="${NODE_LETSENCRYPT:-false}"

roost_dep_install() {
  output "Installing dependencies for roost ($OS $OS_VER)..."

  [ "$CONFIGURE_FIREWALL" == true ] && install_firewall

  # Docker already usable (docker-ce or docker.io): do not touch it.
  if command -v docker >/dev/null 2>&1 && systemctl is-active --quiet docker 2>/dev/null; then
    output "Docker is already installed and running ($(docker --version 2>/dev/null | awk '{print $3}'))"
    success "Dependencies checked"
    return 0
  fi

  install_packages "ca-certificates gnupg lsb-release" true

  mkdir -p /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/debian/gpg | gpg --dearmor --yes -o /etc/apt/keyrings/docker.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/$OS \
$(lsb_release -cs) stable" | tee /etc/apt/sources.list.d/docker.list >/dev/null

  update_repos true

  install_packages "docker-ce docker-ce-cli containerd.io" true

  systemctl start docker >/dev/null 2>&1 || true
  systemctl enable docker >/dev/null 2>&1 || true

  success "Dependencies installed!"
}

roost_dl() {
  if [ -n "$ROOST_BINARY" ]; then
    output "Installing local roost binary (${ROOST_BINARY}).."
    [ -x "$ROOST_BINARY" ] || { error "ROOST_BINARY is not an executable file: $ROOST_BINARY"; exit 1; }
    install -m 755 "$ROOST_BINARY" "$ROOST_BIN"
    success "roost installed from local binary"
    return 0
  fi

  local url="${ROOST_DL_BASE_URL}${ARCH}"
  echo "* Downloading roost.. "
  curl -fsSL -o "$ROOST_BIN.tmp" "$url" || {
    error "Could not download roost from $url"
    error "Publish a release or set ROOST_BINARY=/path/to/roost"
    exit 1
  }
  if curl -fsSL -o "$ROOST_BIN.tmp.sha256" "${url}.sha256" 2>/dev/null; then
    echo "$(awk '{print $1}' "$ROOST_BIN.tmp.sha256")  $ROOST_BIN.tmp" | sha256sum -c - >/dev/null 2>&1 \
      || { error "Checksum verification failed!"; rm -f "$ROOST_BIN.tmp" "$ROOST_BIN.tmp.sha256"; exit 1; }
    rm -f "$ROOST_BIN.tmp.sha256"
  fi
  install -m 755 "$ROOST_BIN.tmp" "$ROOST_BIN"
  rm -f "$ROOST_BIN.tmp"
  success "roost downloaded successfully"
}

roost_systemd() {
  output "Installing systemd service.."
  cat > /etc/systemd/system/roost.service <<'EOF'
[Unit]
Description=Pterodactyl Roost Daemon (Wings-compatible)
After=docker.service
Requires=docker.service
PartOf=docker.service

[Service]
User=root
WorkingDirectory=/etc/pterodactyl
LimitNOFILE=4096
Environment=ROOST_CONFIG=/etc/pterodactyl/config.yml
ExecStart=/usr/local/bin/roost
Restart=on-failure
StartLimitInterval=180
StartLimitBurst=30
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable roost >/dev/null 2>&1
  success "Installed systemd service!"
}

roost_dirs() {
  mkdir -p /etc/pterodactyl \
    /var/log/pterodactyl \
    /var/lib/pterodactyl/volumes \
    /var/lib/pterodactyl/archives \
    /var/lib/pterodactyl/backups \
    /tmp/pterodactyl
}

# Decide how the panel reaches this daemon: reuse an existing certificate
# for the node FQDN, obtain one via certbot when the user opted in, or fall
# back to plain HTTP.
node_letsencrypt() {
  output "Configuring Let's Encrypt for the node (${NODE_FQDN}).."
  if ! [ -x "$(command -v certbot)" ]; then
    install_packages "certbot python3-certbot-nginx" true
  fi

  FAILED=false
  certbot certonly --nginx --no-eff-email --email "$email" -d "$NODE_FQDN" || FAILED=true

  if [ ! -d "/etc/letsencrypt/live/$NODE_FQDN/" ] || [ "$FAILED" == true ]; then
    warning "The process of obtaining a Let's Encrypt certificate for the node failed!"
    return 1
  fi
  success "The process of obtaining a Let's Encrypt certificate succeeded!"
}

node_fqdn_and_scheme() {
  # Explicit node FQDN (prompt / env) wins; default is the panel FQDN
  # since panel and daemon share this machine.
  [ -n "$NODE_FQDN" ] || NODE_FQDN="$FQDN"
  NODE_SCHEME="http"

  if [ -d "/etc/letsencrypt/live/$NODE_FQDN/" ]; then
    NODE_SCHEME="https"
    output "Reusing existing Let's Encrypt certificate for $NODE_FQDN"
    return 0
  fi

  if [ "$NODE_LETSENCRYPT" == true ]; then
    if node_letsencrypt; then
      NODE_SCHEME="https"
      return 0
    fi
    warning "The daemon will use HTTP for its own API."
    return 0
  fi

  if [[ $(invalid_ip "$NODE_FQDN") == 1 ]]; then
    output "No certificate for $NODE_FQDN; the daemon will use HTTP for its own API (panel can still connect)."
  fi
}

db_scalar() {
  mariadb -N -s -u root -e "SELECT $1 FROM \`$MYSQL_DB\`.$2 WHERE $3 LIMIT 1;" 2>/dev/null || true
}

create_location_and_node() {
  local loc_id node_id

  loc_id=$(db_scalar "id" "locations" "short='$LOC_SHORT'")
  if [ -n "$loc_id" ]; then
    output "Location '$LOC_SHORT' already exists (id=$loc_id), reusing it"
  else
    output "Creating location '$LOC_SHORT'.."
    loc_id=$(cd "$PANEL_DIR" && php artisan p:location:make \
      --short="$LOC_SHORT" --long="$LOC_LONG" 2>/dev/null |
      grep -oP 'ID of \K[0-9]+' || true)
    [ -n "$loc_id" ] || { error "Could not create location '$LOC_SHORT'"; exit 1; }
    success "Location '$LOC_SHORT' created (id=$loc_id)"
  fi

  node_id=$(db_scalar "id" "nodes" "name='$NODE_NAME'")
  if [ -n "$node_id" ]; then
    output "Node '$NODE_NAME' already exists (id=$node_id), reusing it"
  else
    output "Creating node '$NODE_NAME' (${NODE_SCHEME}://${NODE_FQDN}:8080).."
    node_id=$(cd "$PANEL_DIR" && php artisan p:node:make \
      --name="$NODE_NAME" \
      --description="Auto-provisioned by pterodactyl-roost-installer" \
      --locationId="$loc_id" \
      --fqdn="$NODE_FQDN" \
      --public=1 \
      --scheme="$NODE_SCHEME" \
      --proxy=0 \
      --maintenance=0 \
      --maxMemory="$(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo)" \
      --overallocateMemory=0 \
      --maxDisk="$(df -Pm / | awk 'NR==2 {print $4}')" \
      --overallocateDisk=0 \
      --uploadSize=100 \
      --daemonListeningPort=8080 \
      --daemonSFTPPort=2022 \
      --daemonBase=/var/lib/pterodactyl/volumes 2>/dev/null |
      grep -oP 'id of \K[0-9]+' || true)
    [ -n "$node_id" ] || { error "Could not create node '$NODE_NAME'"; exit 1; }
    success "Node '$NODE_NAME' created (id=$node_id)"
  fi

  NODE_ID="$node_id"
}

write_roost_config() {
  output "Writing ${ROOST_CONFIG_PATH} from panel (p:node:configuration).."
  (cd "$PANEL_DIR" && php artisan p:node:configuration "$NODE_ID" --format=yaml) > "$ROOST_CONFIG_PATH"

  grep -q "^token_id:" "$ROOST_CONFIG_PATH" \
    || { error "Generated config looks wrong"; exit 1; }
  success "Daemon configuration written (node id=$NODE_ID)"
}

verify_roost() {
  output "Starting roost and waiting for its ports.."
  systemctl restart roost >/dev/null 2>&1 || true

  local waited=0
  until curl -sk -o /dev/null --max-time 2 "${NODE_SCHEME}://${NODE_FQDN}:8080/api/system" 2>/dev/null; do
    waited=$((waited + 2))
    [ "$waited" -ge 45 ] && break
    sleep 2
  done

  if ! systemctl is-active --quiet roost; then
    warning "roost is not running — check: journalctl -u roost -f"
    return 1
  fi
  success "roost service is running"

  local ncode
  ncode=$(curl -sk -o /dev/null -w '%{http_code}' "${NODE_SCHEME}://${NODE_FQDN}:8080/api/system" || true)
  if [ "$ncode" == "401" ] || [ "$ncode" == "403" ]; then
    success "Daemon reachable at ${NODE_SCHEME}://${NODE_FQDN}:8080 (auth enforced)"
  else
    warning "Daemon check returned HTTP $ncode (ports 8080/2022 open?)"
  fi

  # Node health as the panel sees it (the same request the admin UI makes).
  local panel_health
  panel_health=$(cd "$PANEL_DIR" && php artisan tinker --execute='try { $d = app()->make(\Pterodactyl\Repositories\Wings\DaemonConfigurationRepository::class)->setNode(\Pterodactyl\Models\Node::findOrFail('"$NODE_ID"'))->getSystemInformation(); echo "ONLINE ".json_encode($d); } catch (\Throwable $e) { echo "OFFLINE: ".$e->getMessage(); }' 2>/dev/null | grep -a "^ONLINE\|^OFFLINE" | tail -1 || true)

  if [[ "$panel_health" == *ONLINE* ]]; then
    success "Panel reports node '$NODE_NAME' as online (green)"
  else
    warning "Panel could not fetch node status: $panel_health"
  fi
}

perform_install_roost() {
  output "Installing roost.."
  roost_dep_install
  check_virt
  roost_dl
  roost_dirs
  node_fqdn_and_scheme
  create_location_and_node
  write_roost_config
  roost_systemd
  verify_roost || true

  return 0
}

# -------------- Roost user input --------------- #

roost_summary() {
  print_brake 62
  output "Roost daemon with docker on $OS"
  output "Node: $NODE_NAME (${NODE_SCHEME:-http}://${NODE_FQDN:-auto}:8080)"
  output "Node id: ${NODE_ID:-auto}"
  output "Location: $LOC_SHORT"
  output "Config: $ROOST_CONFIG_PATH (generated from panel)"
  output "Configure node Let's Encrypt? $NODE_LETSENCRYPT"
  output "Configure Firewall? $CONFIGURE_FIREWALL"
  print_brake 62
}

ask_roost_firewall() {
  echo -e -n "* Do you want to automatically configure UFW (firewall)? (y/N): "
  read -r CONFIRM_UFW
  [[ "$CONFIRM_UFW" =~ [Yy] ]] && CONFIGURE_FIREWALL=true
  true
}

roost_goodbye() {
  echo ""
  print_brake 70
  echo "* Roost installation completed"
  echo "*"
  echo "* The node was created and configured automatically:"
  echo "*   location '$LOC_SHORT' -> node '$NODE_NAME' (id=${NODE_ID})"
  echo "*   daemon config written to $ROOST_CONFIG_PATH"
  echo "*"
  echo "* roost is running as a systemd service:"
  echo "*   systemctl status roost"
  echo "*   journalctl -u roost -f"
  echo "*"
  echo "* Diagnostics:"
  echo "*   roost diagnostics"
  echo -e "* ${COLOR_RED}Note${COLOR_NC}: It is recommended to enable swap (for Docker)."
  [ "$CONFIGURE_FIREWALL" == false ] && echo -e "* ${COLOR_RED}Note${COLOR_NC}: If you haven't configured your firewall, ports 8080 and 2022 needs to be open."
  print_brake 70
  echo ""
}

# ================== UI ======================== #

collect_panel_answers() {
  # Headless mode: all variables must be provided via environment.
  if [ "${SKIP_PROMPTS:-false}" == "true" ]; then
    [ -n "$FQDN" ] || { error "SKIP_PROMPTS=true requires FQDN"; exit 1; }
    [ -n "$email" ] || { error "SKIP_PROMPTS=true requires email"; exit 1; }
    [ -n "$user_email" ] && [ -n "$user_username" ] && [ -n "$user_firstname" ] \
      && [ -n "$user_lastname" ] && [ -n "$user_password" ] \
      || { error "SKIP_PROMPTS=true requires user_* variables"; exit 1; }
    [ -n "$MYSQL_PASSWORD" ] || MYSQL_PASSWORD="$(gen_passwd 64)"
    [ -n "$telemetry" ] || telemetry="false"
    welcome "panel"
    panel_summary
    return 0
  fi

  welcome "panel"

  # check if we can detect an already existing installation
  if [ -d "$PANEL_DIR" ]; then
    warning "The script has detected that you already have Pterodactyl panel on your system! You cannot run the script multiple times, it will fail!"
    echo -e -n "* Are you sure you want to proceed? (y/N): "
    read -r CONFIRM_PROCEED
    if [[ ! "$CONFIRM_PROCEED" =~ [Yy] ]]; then
      error "Installation aborted!"
      exit 1
    fi
  fi

  output "Database configuration."
  output ""
  output "This will be the credentials used for communication between the MySQL"
  output "database and the panel. You do not need to create the database"
  output "before running this script, the script will do that for you."
  output ""

  [ -n "$MYSQL_PASSWORD" ] || MYSQL_PASSWORD="$(gen_passwd 64)"

  readarray -t valid_timezones < <(timedatectl list-timezones 2>/dev/null)
  output "List of valid timezones here $(hyperlink "https://www.php.net/manual/en/timezones.php")"

  while [ -z "$timezone" ]; do
    echo -n "* Select timezone [Asia/Jakarta]: "
    read -r timezone_input

    array_contains_element "$timezone_input" "${valid_timezones[@]}" && timezone="$timezone_input"
    [ -z "$timezone_input" ] && timezone="Asia/Jakarta"
  done

  email_input email "Provide the email address that will be used to configure Let's Encrypt and Pterodactyl: " "Email cannot be empty or invalid"

  # Initial admin account
  email_input user_email "Email address for the initial admin account: " "Email cannot be empty or invalid"
  required_input user_username "Username for the initial admin account: " "Username cannot be empty" "admin"
  required_input user_firstname "First name for the initial admin account: " "Name cannot be empty" "Admin"
  required_input user_lastname "Last name for the initial admin account: " "Name cannot be empty" "User"
  password_input user_password "Password for the initial admin account: " "Password cannot be empty" "$(gen_passwd 24)"

  print_brake 72

  # set FQDN
  local default_fqdn="${FQDN:-}"
  [ -z "$default_fqdn" ] && default_fqdn=$(curl -4 -fs --max-time 10 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')
  required_input FQDN "Set the FQDN of this panel (panel.example.com): " "" "$default_fqdn"

  # Check if SSL is available
  check_FQDN_SSL

  # Ask if firewall is needed
  [ -z "$CONFIGURE_FIREWALL" ] && CONFIGURE_FIREWALL=false
  if [ "$CONFIGURE_FIREWALL" == false ]; then
    ask_firewall CONFIGURE_FIREWALL
  fi

  # Only ask about SSL if it is available
  if [ "$SSL_AVAILABLE" == true ]; then
    ask_letsencrypt
    [ "$CONFIGURE_LETSENCRYPT" == false ] && ask_assume_ssl
  fi

  # verify FQDN if the user selected Let's Encrypt or assume SSL
  if [ "$CONFIGURE_LETSENCRYPT" == true ] || [ "$ASSUME_SSL" == true ]; then
    verify_fqdn "$FQDN"
  fi

  # ask telemetry preference
  [ -z "$telemetry" ] && ask_telemetry

  # summary
  panel_summary

  # confirm installation
  echo -e -n "\n* Initial configuration completed. Continue with installation? (y/N): "
  read -r CONFIRM
  if [[ ! "$CONFIRM" =~ [Yy] ]]; then
    error "Installation aborted."
    exit 1
  fi
}

collect_roost_answers() {
  # Headless mode.
  if [ "${SKIP_PROMPTS:-false}" == "true" ]; then
    welcome "roost"
    NODE_FQDN="${NODE_FQDN:-}"  # computed later
    return 0
  fi

  welcome "roost"

  if [ -f "$ROOST_CONFIG_PATH" ] && [ -x "$ROOST_BIN" ]; then
    warning "The script has detected an existing roost installation on this system; the binary and service will be upgraded in place."
  fi

  echo "* "
  echo "* The installer will install Docker and roost, then connect it to the"
  echo "* panel on this machine: it creates the location and node, generates"
  echo "* /etc/pterodactyl/config.yml from the panel and starts the daemon."
  echo "* "
  print_brake 42

  ask_roost_firewall

  # Node FQDN + SSL (mirrors the wings installer flow)
  local default_node_fqdn="${NODE_FQDN:-$FQDN}"
  if [ -z "$default_node_fqdn" ] || [ "$default_node_fqdn" == "localhost" ]; then
    default_node_fqdn=$(curl -4 -fs --max-time 10 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')
  fi
  required_input NODE_FQDN "Set the FQDN of this node (node.example.com): " "" "$default_node_fqdn"

  NODE_LETSENCRYPT=false
  if [[ $(invalid_ip "$NODE_FQDN") == 1 && "$NODE_FQDN" != "localhost" ]]; then
    if [ -d "/etc/letsencrypt/live/$NODE_FQDN/" ]; then
      output "A Let's Encrypt certificate for $NODE_FQDN already exists; it will be reused."
    else
      echo -e -n "* Do you want to automatically configure HTTPS using Let's Encrypt? (y/N): "
      read -r CONFIRM_NODE_SSL
      if [[ "$CONFIRM_NODE_SSL" =~ [Yy] ]]; then
        NODE_LETSENCRYPT=true
        [ -z "$email" ] && email_input email "Email address for Let's Encrypt: " "Email cannot be empty or invalid"
      fi
    fi
  else
    warning "Let's Encrypt is not available for IP addresses; the daemon API will use HTTP."
  fi

  roost_summary

  echo -e -n "\n* Proceed with installation? (y/N): "
  read -r CONFIRM
  if [[ ! "$CONFIRM" =~ [Yy] ]]; then
    error "Installation aborted."
    exit 1
  fi
}

show_menu() {
  echo -e "${COLOR_CYAN}"
  cat <<'BANNER'
  ____   ___    _  _____     _             _
 |  _ \ | _ \  / \|_   _|   / \   _ __ ___| |__   ___ _ __
 | |_) ||   / / _ \ | |    / _ \ | '__/ __| '_ \ / _ \ '__|
 |  __/ | | \/ ___ \| |   / ___ \| | | (__| | | |  __/ |
 |_|    |_|_/_/   \_\_|  /_/   \_\_|  \___|_| |_|\___|_|
BANNER
  echo -e "${COLOR_NC}"

  options=(
    "Install the panel AND roost on the same machine (node auto-configured)"
    "Install the panel only"
    "Install roost only (requires an existing panel; auto-configures the node)"
    "Exit"
  )

  output "What would you like to do?"

  for i in "${!options[@]}"; do
    output "[$i] ${options[$i]}"
  done

  echo -n "* Input 0-$((${#options[@]} - 1)): "
  read -r action

  [ -z "$action" ] && error "Input is required" && return 1

  case "$action" in
  0)
    collect_panel_answers
    perform_install_panel
    collect_roost_answers
    perform_install_roost
    panel_goodbye
    roost_goodbye
    save_install_info
    ;;
  1)
    collect_panel_answers
    perform_install_panel
    panel_goodbye
    save_install_info
    ;;
  2)
    collect_roost_answers
    perform_install_roost
    roost_goodbye
    save_install_info
    ;;
  3)
    output "Bye."
    exit 0
    ;;
  *)
    error "Invalid option"
    return 1
    ;;
  esac
}

panel_goodbye() {
  echo ""
  print_brake 62
  output "Panel installation completed"
  output ""

  local app_url="http://$FQDN"
  { [ "$ASSUME_SSL" == true ] || [ "$CONFIGURE_LETSENCRYPT" == true ]; } && app_url="https://$FQDN"
  output "Your panel should be accessible from $(hyperlink "$app_url")"
  output ""
  output "Installation is using nginx on $OS"
  output "Thank you for using this script."
  [ "$CONFIGURE_FIREWALL" == false ] && echo -e "* ${COLOR_RED}Note${COLOR_NC}: If you haven't configured the firewall: 80/443 (HTTP/HTTPS) is required to be open!"
  print_brake 62
  echo ""
}

save_install_info() {
  local app_url="http://$FQDN"
  { [ "$ASSUME_SSL" == true ] || [ "$CONFIGURE_LETSENCRYPT" == true ]; } && app_url="https://$FQDN"
  cat > /root/pterodactyl-install-info.txt <<EOF || true
Pterodactyl Panel + Roost — installation summary
================================================
Panel URL   : ${app_url}
Admin user  : ${user_email:-n/a}
Admin pass  : ${user_password:-n/a}
Timezone    : ${timezone}
Node        : ${NODE_NAME:-n/a} (id=${NODE_ID:-n/a}) -> ${NODE_SCHEME:-http}://${NODE_FQDN:-n/a}:8080
DB          : ${MYSQL_DB} / ${MYSQL_USER} / ${MYSQL_PASSWORD}
roost config: ${ROOST_CONFIG_PATH}

Commands
  systemctl {start,stop,status} roost
  journalctl -u roost -f
  roost diagnostics
EOF
  chmod 600 /root/pterodactyl-install-info.txt 2>/dev/null || true
}

# --------------- lib prechecks ---------------- #

if [[ $EUID -ne 0 ]]; then
  error "This script must be executed with root privileges."
  exit 1
fi

if ! [ -x "$(command -v curl)" ]; then
  echo "* curl is required in order for this script to work."
  echo "* install using apt (Debian and derivatives)"
  exit 1
fi

# Detect OS
if [ -f /etc/os-release ]; then
  . /etc/os-release
  OS=$(echo "$ID" | awk '{print tolower($0)}')
  OS_VER=$VERSION_ID
elif type lsb_release >/dev/null 2>&1; then
  OS=$(lsb_release -si | awk '{print tolower($0)}')
  OS_VER=$(lsb_release -sr)
elif [ -f /etc/debian_version ]; then
  OS="debian"
  OS_VER=$(cat /etc/debian_version)
else
  error "Unsupported OS: cannot detect from /etc/os-release"
  exit 1
fi

OS=$(echo "$OS" | awk '{print tolower($0)}')
OS_VER_MAJOR=$(echo "$OS_VER" | cut -d. -f1)
CPU_ARCHITECTURE=$(uname -m)

case "$CPU_ARCHITECTURE" in
x86_64) ARCH=amd64 ;;
arm64 | aarch64) ARCH=arm64 ;;
*)
  error "Only x86_64 and arm64 are supported!"
  exit 1
  ;;
esac

# --------------- main loop -------------------- #

echo -e "\n\n* pterodactyl-roost-installer $(date) \n\n" >>$LOG_PATH

# Headless: CHOICE=0|1|2 selects the menu option directly.
if [ -n "${CHOICE:-}" ]; then
  case "$CHOICE" in
  0)
    collect_panel_answers
    perform_install_panel
    collect_roost_answers
    perform_install_roost
    panel_goodbye
    roost_goodbye
    save_install_info
    ;;
  1)
    collect_panel_answers
    perform_install_panel
    panel_goodbye
    save_install_info
    ;;
  2)
    collect_roost_answers
    perform_install_roost
    roost_goodbye
    save_install_info
    ;;
  *) error "Invalid CHOICE '$CHOICE' (use 0, 1 or 2)"; exit 1 ;;
  esac
  exit 0
fi

while true; do
  show_menu || continue
  break
done
