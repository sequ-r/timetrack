# TimeTrack

One person tracking their own time on their own machines, to see where a week went and hand a stakeholder an export.

## Language

**Entry**:
A closed interval of tracked time attributed to one project.
_Avoid_: interval, row, timesheet

**Project**:
A named grouping entries belong to; archiving hides it without removing history.
_Avoid_: tag, client, account, container

**Export**:
A CSV document of every entry in a scope, oldest first, with header row.
_Avoid_: report, dump

**Export scope**:
Whether an export holds a week, a month, or all time.
_Avoid_: range, period

**Anchor**:
The instant selecting which week or month a scoped export covers; ignored by all.
_Avoid_: date, at

**Week**:
The ISO week (Monday to Sunday, local time) containing the anchor.
_Avoid_: calendar week, this week

**Month**:
The calendar month (local time) containing the anchor.
_Avoid_: this month

**Stakeholder**:
The person outside the app who receives an export.
_Avoid_: client, manager

**External source**:
A calendar or task system outside the app whose events and tasks are fetched read-only.
_Avoid_: integration, provider, feed

**Suggestion**:
A fetched event or task offered as a possible entry; it is not tracked time until confirmed.
_Avoid_: import, draft entry
