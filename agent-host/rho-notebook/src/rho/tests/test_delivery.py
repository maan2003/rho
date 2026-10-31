"""The default mail delivery."""

from . import fake_rho
from .support import WorksetCase, add_agent, claude, native, wait_for

import rho


def mail(state, id, sender, to, text):
    state.mails.append({"id": id, "at": 0, "sender": sender, "to": to, "re": None, "kind": None, "text": text})


class Delivery(WorksetCase):
    def seed(self, state):
        self.a = add_agent(state, native(), preset="engineer", cwd="/")
        self.b = add_agent(state, native(), preset="child", parent=self.a, cwd="/")
        self.adv = add_agent(state, native("astra", "xhigh"), preset="advisor", parent=self.a, cwd="/")
        mail(state, 1, "user", self.a, "first")
        mail(state, 2, "user", self.a, "second")
        mail(state, 3, self.b, self.a, "from the child")
        mail(state, 4, "user", self.b, "to the child")
        mail(state, 5, self.adv, self.a, "advice")
        mail(state, 6, "user", self.a, "third")
        # The log already holds the first two as inputs to `a`.
        state.inputs += [(self.a, "first", "person", 1), (self.a, "second", "person", 2)]

    def inputs(self, agent):
        return [(text, wake, mail) for a, text, wake, mail in self.state.inputs if a == agent]

    def test_resumes_after_what_the_log_says_was_delivered_in_order(self):
        wait_for(lambda: len(self.inputs(self.a)) == 5, "delivery to a")
        self.assertEqual(self.inputs(self.a), [
            ("first", "person", 1),
            ("second", "person", 2),
            (f"Message Type: MESSAGE\nSender: eng-{self.b}\nPayload:\nfrom the child", "agent", 3),
            (f"Message Type: MESSAGE\nSender: adv-{self.adv}\nPayload:\nadvice\n\nAdvisor adv-{self.adv} remains available. Use mail.send with this ID to continue.", "agent", 5),
            ("Message from the user:\nthird", "person", 6),
        ])
        wait_for(lambda: self.inputs(self.b), "delivery to b")
        self.assertEqual(self.inputs(self.b), [("Message from the user:\nto the child", "person", 4)])
        self.assertEqual(self.inputs(self.adv), [])

    def test_later_mail_and_later_agents(self):
        wait_for(lambda: len(self.inputs(self.a)) == 5, "delivery to a")
        id = fake_rho.post_user_mail(self.a, "later")
        wait_for(lambda: len(self.inputs(self.a)) == 6, "later mail")
        self.assertEqual(self.inputs(self.a)[-1], ("Message from the user:\nlater", "person", id))
        output, error = rho._eval(
            f"c = rho.agents.create({claude()!r}, {{\'preset\': \'child\', \'parent\': {self.a!r}, \'cwd\': \'/\'}})\n"
            f"rho.mail.send({self.a!r}, c.id, \'go\')\nprint(c.id)"
        )
        self.assertIsNone(error)
        new = output.strip()
        wait_for(lambda: self.inputs(new), "delivery to a new agent")
        self.assertEqual(self.inputs(new), [(f"Message Type: MESSAGE\nSender: eng-{self.a}\nPayload:\ngo", "agent", id + 1)])

    def test_a_failed_input_is_retried_from_the_log(self):
        from unittest import mock

        from rho import default

        wait_for(lambda: len(self.inputs(self.a)) == 5, "delivery to a")
        real = fake_rho.input
        failures = []

        def flaky(agent, text, **kw):
            if not failures:
                failures.append(text)
                raise ConnectionError("host went away")
            real(agent, text, **kw)

        with mock.patch.object(default, "RETRY_SECONDS", 0.01), mock.patch.object(fake_rho, "input", flaky), mock.patch("sys.stderr"):
            id = fake_rho.post_user_mail(self.a, "retry me")
            wait_for(lambda: len(self.inputs(self.a)) == 6, "retried mail")
        self.assertEqual(failures, ["Message from the user:\nretry me"])
        self.assertEqual(self.inputs(self.a)[-1], ("Message from the user:\nretry me", "person", id))
