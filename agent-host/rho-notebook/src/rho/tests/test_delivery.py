"""The default delivery of chat messages."""

from . import fake_rho
from .support import WorksetCase, add_agent, claude, native, wait_for

import rho


def header(topic, name):
    return f"[#{topic} rho › {name}]"


def direct(topic, other):
    return f"[#{topic} direct with {other}]"


class Delivery(WorksetCase):
    def seed(self, state):
        self.a = add_agent(state, native(), preset="engineer", cwd="/")
        self.b = add_agent(state, native(), preset="child", parent=self.a, cwd="/")
        self.adv = add_agent(state, native("astra", "xhigh"), preset="advisor", parent=self.a, cwd="/")
        # Stream 1. The user's topic (2) mentions `a`, which joins it:
        # messages 3 and 4.
        self.mine, _ = fake_rho.start_user_topic("rho", "Fix it", "first", [self.a])
        fake_rho.join(self.mine, self.a)
        fake_rho.post_user(self.mine, "second")
        # `a` briefs `b` in their direct conversation 5 (6); `b` answers (7).
        self.child = fake_rho.direct(self.a, self.b)
        fake_rho.post(self.a, self.child, "do x")
        fake_rho.post(self.b, self.child, "from the child")
        # `a` asks the advisor in 8 (9); it answers (10).
        self.advice = fake_rho.direct(self.a, self.adv)
        fake_rho.post(self.a, self.advice, "q")
        fake_rho.post(self.adv, self.advice, "advice")
        fake_rho.post_user(self.mine, "third")  # 11
        # A mention reaches `b`, which is not in the topic (12).
        fake_rho.post(self.a, self.mine, "b, look", mentions=[(self.b, "fyi")])
        # The log already holds the first two as inputs to `a`.
        state.inputs += [(self.a, "first", "person", 3), (self.a, "second", "person", 4)]

    def inputs(self, agent):
        return [(text, wake, message) for a, text, wake, message in self.state.inputs if a == agent]

    def test_resumes_after_what_the_log_says_was_delivered_in_order(self):
        wait_for(lambda: len(self.inputs(self.a)) == 5, "delivery to a")
        self.assertEqual(self.inputs(self.a), [
            ("first", "person", 3),
            ("second", "person", 4),
            (f"{direct(5, 'eng-' + self.b)}\nMessage Type: MESSAGE\nSender: eng-{self.b}\nPayload:\nfrom the child", "agent", 7),
            (f"{direct(8, 'adv-' + self.adv)}\nMessage Type: MESSAGE\nSender: adv-{self.adv}\nPayload:\nadvice", "agent", 10),
            (f"{header(2, 'Fix it')} Message from the user:\nthird", "person", 11),
        ])
        wait_for(lambda: len(self.inputs(self.b)) == 2, "delivery to b")
        self.assertEqual(self.inputs(self.b), [
            (f"{direct(5, 'eng-' + self.a)}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\ndo x", "agent", 6),
            (f"{header(2, 'Fix it')}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\nb, look", "agent", 12),
        ])
        wait_for(lambda: self.inputs(self.adv), "delivery to the advisor")
        self.assertEqual(self.inputs(self.adv), [
            (f"{direct(8, 'eng-' + self.a)}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\nq", "agent", 9),
        ])

    def test_a_mention_does_not_join(self):
        wait_for(lambda: len(self.inputs(self.b)) == 2, "delivery to b")
        id = fake_rho.post_user(self.mine, "later")
        wait_for(lambda: len(self.inputs(self.a)) == 6, "a later message")
        self.assertEqual(self.inputs(self.a)[-1], (f"{header(2, 'Fix it')} Message from the user:\nlater", "person", id))
        self.assertEqual(len(self.inputs(self.b)), 2)

    def test_the_users_mention_joins_and_the_joins_hook_decides(self):
        wait_for(lambda: len(self.inputs(self.b)) == 2, "delivery to b")
        mention = fake_rho.post_user(self.mine, "b, take this", [self.b])
        wait_for(lambda: len(self.inputs(self.b)) == 3, "the mention")
        self.assertEqual(self.state.topics[self.mine]["agents"], [self.a, self.b])
        later = fake_rho.post_user(self.mine, "and this")
        wait_for(lambda: len(self.inputs(self.b)) == 4, "a later message")
        self.assertEqual([m for *_, m in self.inputs(self.b)[2:]], [mention, later])
        # A policy that never joins: a user's mention only reaches.
        from rho import hooks

        _, error = rho._eval("rho.hooks.joins = lambda agent, record: False")
        self.assertIsNone(error)
        topic, first = fake_rho.start_user_topic("ops", "Other", "adv, look", [self.adv])
        wait_for(lambda: len(self.inputs(self.adv)) == 2, "the mention")
        self.assertEqual(self.state.topics[topic]["agents"], [])
        self.assertIsNot(hooks.joins, rho.default.joins)

    def test_later_agents(self):
        output, error = rho._eval(
            f"c = rho.agents.create({claude()!r}, {{'preset': 'child', 'parent': {self.a!r}, 'cwd': '/'}})\n"
            f"t = rho.chat.direct({self.a!r}, c.id)\n"
            f"print(c.id, t, rho.chat.post({self.a!r}, t, 'go'))"
        )
        self.assertIsNone(error)
        new, topic, message = output.split()
        wait_for(lambda: self.inputs(new), "delivery to a new agent")
        self.assertEqual(self.inputs(new), [(f"{direct(int(topic), 'eng-' + self.a)}\nMessage Type: MESSAGE\nSender: eng-{self.a}\nPayload:\ngo", "agent", int(message))])

    def test_an_agent_started_from_a_message_joins_its_topic_and_starts_from_it(self):
        topic, origin = fake_rho.start_user_topic("ops", "Deploy", "deploy it")
        fake_rho.post_user(topic, "before it exists")
        output, error = rho._eval(f"print(new_agent(cwd='/', runtime={native()!r}, origin='chat:{origin}'))")
        self.assertIsNone(error)
        new = output.strip()
        self.assertEqual(self.state.agents[new]["props"], {"preset": "engineer", "origin": f"chat:{origin}", "cwd": "/"})
        wait_for(lambda: self.inputs(new), "the origin message")
        later = fake_rho.post_user(topic, "and then this")
        wait_for(lambda: len(self.inputs(new)) == 2, "a later message of the topic")
        where = f"[#{topic} ops › Deploy]"
        self.assertEqual(self.inputs(new), [
            (f"{where} Message from the user:\ndeploy it", "person", origin),
            (f"{where} Message from the user:\nand then this", "person", later),
        ])
        self.assertEqual(self.state.topics[topic]["agents"], [new])
        for bad in ["chat:x", "slack:1", f"chat:{later + 50}"]:
            _, error = rho._eval(f"new_agent(cwd='/', runtime={native()!r}, origin={bad!r})")
            self.assertIn("origin", error, bad)

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

        retry = f"{header(2, 'Fix it')} Message from the user:\nretry me"
        with mock.patch.object(default, "RETRY_SECONDS", 0.01), mock.patch.object(fake_rho, "input", flaky), mock.patch("sys.stderr"):
            id = fake_rho.post_user(self.mine, "retry me")
            wait_for(lambda: len(self.inputs(self.a)) == 6, "retried message")
        self.assertEqual(failures, [retry])
        self.assertEqual(self.inputs(self.a)[-1], (retry, "person", id))
