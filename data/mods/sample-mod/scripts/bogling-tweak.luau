-- tessera-api: 1
-- Sample-mod demonstration handler: fires on every Attack and records one
-- observable event-log line, proving a mod-shipped script handler shares the
-- engine's dispatch path. The Bogling blueprint lists this script.
engine.on("Attack", engine.LATE, function(ev)
  engine.log(("bogling tweak: attack on defender %d"):format(ev.defender))
  return true
end)