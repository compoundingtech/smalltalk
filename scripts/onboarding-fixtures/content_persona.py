"""Person-only answers for a private semantic onboarding fixture.

Only current questions belonging to this run's steps can receive an answer.
Actual review reading and graph mutations remain in the eval controller.
"""
class Persona:
 def __init__(self,variant):
  self.variant=variant; self.handled=set(); self.help_used=False; self.decline_used=False

 def answer(self,card,run):
  if card.get("state")!="open" or card.get("id") in self.handled: return None
  blocked=card.get("blocked") or {}
  step=next((s for s in run.get("steps",[]) if s["subject"]==blocked.get("step_run_id") and s["generation"]==run["generation"]),None)
  if step is None or step["status"] in ("completed","cancelled","failed"): return None
  if card.get("person_id")!=run["requester"] or blocked.get("attempt")!=step["attempt"]: return None
  request=card.get("request") or {}; kind=request.get("type"); name=step["step"]
  ids={a["id"] for a in request.get("answers",[])}
  if kind=="feedback" and name=="your-project":
   return {"step":name,"text":"Use /home/ada/garden-project and the installed claude harness. Keep its first private message short; ask my decision before creating the seat."}
  if kind=="choice" and name=="tour" and {"tour-seen","help"}<=ids:
   help_first=self.variant==2 and not self.help_used
   return {"step":name,"answer":"help" if help_first else "tour-seen"}
  if kind=="decision" and name in ("first-mission","your-project") and {"accept","decline"}<=ids:
   if name=="your-project":
    proposal=request.get("question","")+" "+(request.get("summary") or "")
    if "/home/ada/garden-project" not in proposal or "claude" not in proposal.lower(): return None
   decline_first=self.variant==2 and name=="first-mission" and not self.decline_used
   return {"step":name,"answer":"decline" if decline_first else "accept"}
  if kind=="choice" and name in ("phone","second-machine","github") and {"enable","skip"}<=ids:
   return {"step":name,"answer":"skip"}
  return None

 def answered(self,card,decision):
  self.handled.add(card["id"])
  if decision.get("answer")=="help": self.help_used=True
  if decision.get("answer")=="decline": self.decline_used=True
