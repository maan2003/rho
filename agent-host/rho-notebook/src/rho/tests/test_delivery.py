"""The default delivery of chat messages."""

from . import fake_rho
from .support import WorksetCase, add_agent, claude, native, wait_for

import rho


def header(topic, name):
    return f"[#{topic} rho › {name}]"


class Delivery(WorksetCase):
    def seed(self, state):
        self.a = add_agent(state, native(), preset="engineer", cwd="/")
        self.b = add_agent(state, native(), preset="child", parent=self.a, cwd="/")
        self.adv = add_agent(state, native("astra", "xhigh"), preset="advisor", parent=self.a, cwd="/")
        # The user's topic with `a` (2): messages 3 and 4.
        self.mine = fake_rho.start_user_topic("Fix it", "first", [self.a])
        fake_rho.post_user(self.mine, "second")
        # `a` briefs `b` in topic 5 (6); `b` answers (7); the user writes
        # there too (8), so both get it.
        self.child = fake_rho.start_topic(self.a, "rho", "help", "do x", agents=[self.a, self.b])
        fake_rho.post(self.b, self.child, "from the child")
        fake_rho.post_user(self.child, "to the child")
        # `a` asks the advisor in topic 9 (10); it answers (11).
        self.advice = fake_rho.start_topic(self.a, "rho", "Advisor: q", "q", agents=[self.a, self.adv])
        fake_rho.post(self.adv, self.advice, "advice")
        fake_rho.post_user(self.mine, "third")  # 12
        # A mention reaches `b`, which is not in the topic (13).
        fake_rho.post(self.a, self.mine, "b, look", mentions=[(self.b, "fyi")])
        # The log already holds the first two as inputs to `a`.
        state.inputs += [(self.a, "first", "person", 3), (self.a, "second", "person", 4)]

    def inputs(self, agent):
        return [(text, wake, message) for a, text, wake, message in self.state.inputs if a == agent]

    def test_resumes_after_what_the_log_says_was_delivered_in_order(self):
        wait_for(lambda: len(self.inputs(self.a)) == 6, "delivery to a")
        self.assertEqual(self.inputs(self.a), [
            ("first", "person", 3),
            ("second", "person", 4),
            (f"{header(5, 'help')}\nMessage Type: MESSAGE\nSender: eng-{self.b}\nPayload:\nfrom the child", "agent", 7),
            (f"{header(5, 'help')} Message from the user:\nto the child", "person", 8),
            (f"{header(9, 'Advisor: q')}\nMessage Type: MESSAGE\nSender: adv-{self.adv}\nPayload:\nadvice", "agent", 11),
            (f"{header(2, 'Fix it')} Message from the user:\nthird", "person", 12),
        ])
        wait_for(lambda: len(self.inputs(self.b)) == 3, "delivery to b")
        self.assertEqual(self.inputs(self.b), [
            (f"{header(5, 'help')}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\ndo x", "agent", 6),
            (f"{header(5, 'help')} Message from the user:\nto the child", "person", 8),
            (f"{header(2, 'Fix it')}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\nb, look", "agent", 13),
        ])
        wait_for(lambda: self.inputs(self.adv), "delivery to the advisor")
        self.assertEqual(self.inputs(self.adv), [
            (f"{header(9, 'Advisor: q')}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\nq", "agent", 10),
        ])

    def test_later_messages_and_later_agents(self):
        wait_for(lambda: len(self.inputs(self.a)) == 6, "delivery to a")
        id = fake_rho.post_user(self.mine, "later")
        wait_for(lambda: len(self.inputs(self.a)) == 7, "a later message")
        self.assertEqual(self.inputs(self.a)[-1], (f"{header(2, 'Fix it')} Message from the user:\nlater", "person", id))
        output, error = rho._eval(
            f"c = rho.agents.create({claude()!r}, {{\'preset\': \'child\', \'parent\': {self.a!r}, \'cwd\': \'/\'}})\n"
            f"print(c.id, rho.chat.start({self.a!r}, \'new\', \'go\', agents=[c.id]))"
        )
        self.assertIsNone(error)
        new, topic = output.split()
        wait_for(lambda: self.inputs(new), "delivery to a new agent")
        self.assertEqual(self.inputs(new), [(f"{header(int(topic), 'new')}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\ngo", "agent", int(topic) + 1)])

    def test_a_failed_input_is_retried_from_the_log(self):
        from unittest import mock

        from rho import default

        wait_for(lambda: len(self.inputs(self.a)) == 6, "delivery to a")
        real = fake_rho.input
        failures = []

        def flaky(agent, text, **kw):
            if not failures:
                failures.append(text)
                raise ConnectionError("host went away")
            real(agent, text, **kw)

        retry = f"{header(2, 'Fix it')} Message from the user:\nretry me"
        with mock.patch.object(default, "RETRY_SECONDS", 0.01), mock.patch.object(fake_rho, "input", flaky), mock.patch("sys.stderr"):
            id = fake_rho.post_user(self.mine, "retry me")
            wait_for(lambda: len(self.inputs(self.a)) == 7, "retried message")
        self.assertEqual(failures, [retry])
        self.assertEqual(self.inputs(self.a)[-1], (retry, "person", id))
