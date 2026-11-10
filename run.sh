#!/usr/bin/with-contenv bashio

export OPTIONS_PATH="/data/options.json"
if bashio::config.has_value 'log_level'; then
    export RUST_LOG="info,ort=warn,energy_planner=$(bashio::config 'log_level')"
fi
exec /usr/bin/energy-planner
