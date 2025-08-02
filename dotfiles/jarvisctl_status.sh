#!/usr/bin/env bash
# jarvisctl-status.sh

output=$(/home/rootster/.local/bin/jarvisctl list 2>/dev/null)

########################
# section extraction   #
########################
namespaces=$(awk '
    /NAMESPACES:/       {in_ns=1; next}
    /AGENTS:/           {in_ns=0}
    in_ns && NF         {print}
' <<<"$output" | grep -v '^(none)$')

agents=$(awk '
    /AGENTS:/           {in_ag=1; next}
    in_ag && NF         {print}
' <<<"$output" | grep -v '^(none)$')

########################
# line counts          #
########################
ns_count=$(grep -c '^[^[:space:]]' <<<"$namespaces")
agent_count=$(grep -c '^[^[:space:]]' <<<"$agents")

########################
# prepend icons to sections
########################
tooltip=" NAMESPACES:\n"
if [[ $ns_count -gt 0 ]]; then
  tooltip+=$(printf '%s\n' "$namespaces")
else
  tooltip+="(none)\n"
fi

tooltip+="\n AGENTS:\n"
if [[ $agent_count -gt 0 ]]; then
  tooltip+=$(printf '%s\n' "$agents")
else
  tooltip+="(none)\n"
fi

########################
# escape for JSON
########################
escaped_tooltip=$(echo "$tooltip" | sed ':a;N;$!ba;s/"/\\"/g;s/\n/\\n/g')

########################
# JSON output
########################
text="  $ns_count  $agent_count"
echo "{\"text\":\"$text\",\"tooltip\":\"$escaped_tooltip\"}"
