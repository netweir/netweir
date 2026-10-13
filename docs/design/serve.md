# Netweir serve

Status: draft, October 2026
Author: James Conway

## What it is for

People who run Scrapy in production mostly run it under scrapyd: an HTTP
service that takes "run this spider" as a job, queues it, runs it in a
process of its own, and keeps its log and items for later. Their
dashboards (ScrapydWeb) and deploy scripts (scrapyd-client) talk to that
API. Netweir has `netweir crawl` and nothing that sits above it.

`netweir serve` is that service for Netweir spiders, small enough to read
in one sitting. It speaks scrapyd's JSON API where doing so costs little,
so a client written for scrapyd mostly works unchanged, and it runs each
job through `netweir crawl`, so a job behaves exactly as it would from the
shell.

## The command

```
netweir serve [--bind 127.0.0.1] [--port 6800] [--dir DIR] [--max-jobs N]
              [--keep N] [--project NAME=PATH ...] [--username U --password P]
```

- `--dir` (default `./netweir-serve`) holds everything the service
  writes: `jobs.db`, `logs/`, `items/` and `stats/`.
- `--max-jobs` (default: the number of CPUs) is how many jobs run at once.
  scrapyd's default is four per CPU; a Netweir crawl already runs its
  fetching and parsing on several threads, so one per CPU is plenty.
- `--keep` (default 100, scrapyd's `finished_to_keep`) is how many
  finished jobs are remembered. Past that the oldest is forgotten, and its
  log, items and stats files are deleted with it, so the directory
  doesn't grow without end.
- `--project NAME=PATH`, repeated: a project is a spider file, or a
  directory whose `.py` files (not those in subdirectories) are spider
  files. A bare `PATH` is named after the file or directory.
- `--username`/`--password`, or `NETWEIR_SERVE_USERNAME` and
  `NETWEIR_SERVE_PASSWORD`: HTTP basic auth on every request. Both or
  neither. The docs recommend the variables: a command line shows in
  `ps` to every user of the machine.
- `--insecure-no-auth`: without it, a `--bind` that isn't a loopback
  address needs a username and password, and the service refuses to
  start.

## Projects and spiders

Netweir has no eggs to deploy: a spider is a class in a Python file. So a
project is files on the service's disk, named on its command line. A
spider's name is what `netweir crawl --spider` matches: its `name`, or
its class name.

Finding the spiders means importing the files, which runs their code. That
happens in a short-lived child process (`python -m netweir._serve
PATH...`), never in the service, so a file that crashes, hangs or
leaks can't take the service down; it's given 60 seconds. It's done again
for every `listspiders.json` and `schedule.json`, so an edited file is
seen at once. A name defined in two files of one project is refused at
`schedule.json`, naming both.

`addversion.json` isn't supported; `listversions.json` answers with an
empty list, for clients that ask before scheduling. Uploading code over HTTP is the one
thing in scrapyd's API that runs whatever the network sends, and Netweir
projects are plain files that can be copied into place with whatever
deploys the rest of your code. `delproject.json` answers with an error
for the same reason: a project comes from the command line, so that's
where it's removed.

## API

Every answer is JSON with `node_name` and `status` (`"ok"` or
`"error"`, with a `message`), as scrapyd's are. Parameters come from the
query string or a form-encoded POST body, of at most 1 MiB. A wrong
parameter is a 400, a wrong path a 404, a wrong method a 405, missing
auth a 401, a refused origin or host a 403, a body too big a 413.

| Endpoint | Method | Parameters | Answer |
|---|---|---|---|
| `daemonstatus.json` | GET | | `pending`, `running`, `finished` counts |
| `listprojects.json` | GET | | `projects` |
| `listspiders.json` | GET | `project` | `spiders` |
| `schedule.json` | POST | `project`, `spider`, `setting=K=V` (repeated), `jobid`, `priority`, any other `name=value` | `jobid` |
| `cancel.json` | POST | `project`, `job` | `prevstate`: `"pending"`, `"running"` or null |
| `listjobs.json` | GET | `project` (optional) | `pending`, `running`, `finished` lists |
| `jobstats.json` | GET | `job` | the job's `state`, `outcome` and `stats` |
| `delproject.json` | POST | `project` | always an error; see above |
| `/logs/P/S/J.log` | GET | | the job's log |
| `listversions.json` | GET | `project` | `versions`, always empty |
| `/items/P/S/J.jl`, `.jsonl` | GET | | the job's items |

`schedule.json`:

- `setting=K=V` is a Netweir setting, as `-s K=V` on the command line,
  checked when the job is scheduled: an unknown setting or bad value is
  an error then, not a failed job later. The parsed settings are built
  into a `Settings` and an engine, so what those refuse (`concurrency=0`,
  `min_delay` over `max_delay`, an unknown `profile`, `browser` or
  `proxy_rotation`) is refused too. The service checks each value's
  type; the child process that lists the spiders, which already has the
  spider's class, applies them over the spider's own settings, as
  `netweir crawl -s` does, and makes the engine, with every path setting
  moved into a scratch directory thrown away after, so nothing is
  created, and without the `queue`, so nothing connects to Redis. Scrapy's setting names aren't translated.
- `checkpoint`, `cache`, `deltafetch`, `warc` and `har`, the settings
  that name a file or directory, must be relative paths without `..`,
  `:` (a URL or a drive) or a backslash. They're resolved inside the
  project's data directory, `DIR/data/P`, and refused if a symlink takes
  them outside it; checked when the job is scheduled and again when it
  starts. A project-wide directory, not a per-job one, so a cancelled job
  scheduled again with the same checkpoint resumes.
- Every other parameter, except `_version` (ignored, as there are no
  versions), is a spider argument, as `-a name=value`: the spider gets it
  as a string attribute, as Scrapy's spiders do. A name must be a Python
  identifier not starting with `_`, and not an attribute `netweir.Spider`
  defines (`settings`, `pipelines`, `checks`, `allowed_domains`, `parse`,
  ...): an argument is data, never a change of behaviour.
- `jobid` must be new, ignoring case (ids are file names, and macOS and
  Windows ignore case; project names do too); without one, the job gets a uuid as a hex string,
  as scrapyd's do.
- `priority`, a finite number, puts the job ahead of pending jobs with a lower
  one; equal priorities go in the order they came.

The `listjobs.json` entries have scrapyd's fields (`id`, `project`,
`spider`, `pid` while running, `start_time`, `end_time`, `log_url`,
`items_url`, ending `.jl` as scrapyd's do) plus `outcome` on finished
jobs: `"finished"` (exit status
0), `"failed"` (any other), `"cancelled"`, or `"lost"` (it was running when
the service died). Times are local, written as scrapyd writes them
(`2026-10-12 14:03:59.123456`).

## Process model

The service is a `ThreadingHTTPServer` and one scheduler thread. Each job
is

```
python -m netweir crawl FILE --spider NAME -o DIR/items/P/S/J.jsonl
    --stats DIR/stats/P/S/J.json -s K=V ... -a NAME=VALUE ...
```

started with the project's directory as its working directory, stdout
and stderr going to `DIR/logs/P/S/J.log`, in a session of its own so a
Ctrl-C at the service's terminal reaches the service and not every job at
once. A job that crashes or runs out of memory is that process; the
service sees an exit status.

The scheduler wakes when a job is scheduled, and every half
second to reap. It starts pending jobs, highest priority first, while
fewer than `--max-jobs` run.

`netweir crawl` gains three things for this, all useful on their own:

- `--stats FILE` writes the crawl's stats as JSON when it ends, however
  it ends, with `finish_reason` among them (`"interrupted"` after a
  signal, `"error"` after an exception).
- `-a NAME=VALUE` sets a spider attribute, as Scrapy's does; worker
  processes get it too.
- SIGTERM stops a crawl the way Ctrl-C does: callbacks stop, the
  exporters are closed (so the items so far are on disk) and a
  checkpoint records where it got to. Before this, SIGTERM killed it on
  the spot. `spider.stats` holds the stats after any run, interrupted or
  not.

## Cancelling

A pending job is taken off the queue and kept as finished, `cancelled`. A
running one is sent SIGTERM; if it's still running 30 seconds later, it
gets SIGKILL, sent to its whole process group, which its worker
processes share. Cancelling a job that's already being cancelled sends
SIGKILL at once. A job that has already exited but isn't reaped yet isn't
marked cancelled: the reaper records how it really ended. Either way its outcome is `cancelled`, and its items file
keeps what it wrote.

Stopping the service (Ctrl-C or SIGTERM) cancels the jobs it's running the
same way and waits for them, so it can take up to the grace period to
exit. Pending jobs stay pending for next time.

## Persistence

Before anything else, the service takes an exclusive lock on
`DIR/lock` (`flock`, or `msvcrt.locking` on Windows) and refuses to start
if another holds it, so a second service can't mark the first's running
jobs lost. Any other error taking the lock is reported as itself. A file
lock isn't reliable between NFS clients.

`DIR/jobs.db` is SQLite: one row per job with its project, spider,
settings, arguments, priority, state, outcome, pid, exit status, times and
stats. It's written whenever a job changes state. A job leaves the reaper's
list only once its finished row is written, and a job whose `running`
row failed to write is still known to be running, so it isn't started
twice. When the service
starts, pending jobs are queued again, and jobs the table says were
running are marked `lost`: their process was the old service's child and
nobody is watching it now.

## Security

- It listens on 127.0.0.1 unless told otherwise. Bound elsewhere, it
  needs auth, or `--insecure-no-auth`, in which case it logs a warning at
  start, as anyone who can reach the port can run your spiders.
- With auth, every request needs it, logs and items included; the
  password is compared in constant time.
- Every project, spider and job id in a path or parameter must match
  `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, so none can be `..` or contain a
  slash, and files are only ever opened at paths built from those.
- A connection must send its whole request (line, headers and body)
  within 15 seconds of being accepted, and may pause at most 15 seconds
  in it; then its socket is shut. One address may hold 8 connections and
  everyone together 256; more are closed unanswered. What one slow
  address can hold is 8 threads for 15 seconds at a time; many
  addresses together can still fill the 256. A negative or non-numeric
  `Content-Length` is a 400.
- Job ids ignore case in the table; whatever case a request uses, the
  service works with the id as stored.
- Bound to an address other machines can reach without auth, any web
  page in a browser that can reach it can read the job list, logs and
  items through DNS rebinding, since the `Host` check only applies to
  loopback binds. That's why such a bind needs `--insecure-no-auth`.
- A request with an `Origin` header is refused (403), and, bound to a
  loopback address, so is a request whose `Host` isn't a loopback name (`localhost`, in
  any case and with or without a trailing dot, or a loopback address).
  Scrapyd clients send neither; a web page in a local browser that tries
  to POST `schedule.json`, directly or by DNS rebinding, does.
- No code comes from the network: jobs run only the files the command
  line named, and no setting a client gives can make a job write
  outside DIR. (A spider's own code, given an argument, is the
  operator's to trust.)

## Known limits

- If the service is killed outright (SIGKILL, a power cut), its running
  jobs aren't told: they run on unwatched and are marked `lost` at the
  next start even if they finish.
- A job is marked running in memory before the table says so. If that
  write fails and the service then dies before the job ends, the table
  still has it pending, and the next service runs it again.
- On Windows there's no SIGTERM: a cancelled job is stopped at once, and
  its items file has what it had flushed, which is every batch before the
  last.
- No `addversion.json`, `delversion.json` or eggs.

## Testing

Tests start the service in a thread on a free port, with a local site to
crawl, and drive it over HTTP:

- A scheduled job finishes; its items, log and stats are served.
- With `--max-jobs 1`, a second job waits in pending until the first ends.
- Cancelling a running job keeps the items it had written.
- After a restart, finished and pending jobs are still there and a job
  that was running is `lost`.
- Without credentials every endpoint is a 401.
- `..`, slashes and encoded slashes in names and paths are refused.
- An unknown spider, project or setting is an error at `schedule.json`.
- `checkpoint` and `cache` given as an absolute path, with `..`, as a
  URL or through a symlink out of the data directory are refused; a
  relative one lands in `DIR/data/P`.
- Answers have scrapyd's shape.

