-- An lvc plugin in Lua. Functions get the context table (file, files, dir, other_dir, panel, arg) and
-- give instructions back through `lvc`. State kept in locals lives on between calls.

local calls = 0

function hello(ctx)
  calls = calls + 1
  lvc.notify(("hello from Lua, call #%d, on %s"):format(calls, ctx.file or "nothing"))
end

function ask(ctx)
  lvc.input("Your name", "greet", "")
end

function greet(ctx)
  return "hi, " .. ctx.arg .. "!"          -- a returned string is a message
end

function git_status(ctx)
  -- in the background: the interface stays free, the callback gets (code, stdout, stderr)
  lvc.run({ "git", "-C", ctx.dir, "status", "--short", "--branch" }, function(code, out, err)
    if code == 0 then
      lvc.notify((out:match("^[^\n]*")) or "clean")
    else
      lvc.error("git: " .. (err:match("^[^\n]*") or "failed"))
    end
  end)
end

function pick(ctx)
  lvc.menu("Pick", {
    { label = "Go to /tmp", call = "go", arg = "/tmp" },
    { label = "Say it", call = "say", arg = "picked" },
  })
end

function go(ctx) lvc.cd(ctx.arg) end
function say(ctx) lvc.notify(ctx.arg) end

function spin(ctx) while true do end end
