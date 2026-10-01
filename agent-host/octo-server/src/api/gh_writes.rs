//! Explicit GitHub writes and their request types. Reads are relayed in gh.rs.
//! Preserve JSON except for legacy PR/rerun parameters, where null means
//! omitted.
#![allow(dead_code)] // Request fields are read by serde validation, not by handlers.

use serde::{Deserialize, Deserializer, Serialize};

use super::{Handler, HandlerFuture, Request};

// Unlike Option<T>, an absent field and an explicitly supplied null are
// distinct. T determines whether null is allowed; the original bytes are
// forwarded.
enum Optional<T> {
    Missing,
    Present(T),
}

impl<T> Default for Optional<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Optional<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Present)
    }
}

fn required<B: serde::de::DeserializeOwned + 'static>(request: Request) -> HandlerFuture {
    Box::pin(super::typed::<B>(request, true, true))
}

fn optional<B: serde::de::DeserializeOwned + 'static>(request: Request) -> HandlerFuture {
    Box::pin(super::typed::<B>(request, false, true))
}

// Master omitted optional null PR/rerun parameters before sending to GitHub.
// Keep that compatibility local to these types; nullable issue edits stay
// intact.
fn legacy<B: serde::de::DeserializeOwned + Serialize + 'static>(
    mut request: Request,
    body_required: bool,
) -> HandlerFuture {
    Box::pin(async move {
        if !request.body.is_empty() {
            let Ok(body) = serde_json::from_slice::<B>(&request.body) else {
                return super::forbidden();
            };
            request.body = serde_json::to_vec(&body)
                .expect("validated GitHub request serializes")
                .into();
        }
        super::typed::<B>(request, body_required, true).await
    })
}

fn no_body(request: Request) -> HandlerFuture {
    Box::pin(super::typed::<()>(request, false, false))
}

pub(super) fn handler(group: &str, name: &str) -> Option<Handler> {
    Some(match (group, name) {
        ("actions", "re_run_job_for_workflow_run") => {
            |request| legacy::<Option<RerunJob>>(request, false)
        }
        ("actions", "re_run_workflow") => |request| legacy::<Option<RerunWorkflow>>(request, false),
        ("actions", "re_run_workflow_failed_jobs") => {
            |request| legacy::<Option<RerunWorkflow>>(request, false)
        }
        ("issues", "create") => required::<CreateIssue>,
        ("issues", "update") => optional::<UpdateIssue>,
        ("issues", "create_comment") => required::<Comment>,
        ("issues", "update_comment") => required::<Comment>,
        ("issues", "delete_comment") => no_body,
        ("issues", "pin_comment") => no_body,
        ("issues", "unpin_comment") => no_body,
        ("issues", "add_assignees") => optional::<AddAssignees>,
        ("issues", "remove_assignees") => optional::<RemoveAssignees>,
        ("issues", "add_blocked_by_dependency") => required::<Dependency>,
        ("issues", "remove_dependency_blocked_by") => no_body,
        ("issues", "add_issue_field_values") => required::<IssueFields<FieldInput>>,
        ("issues", "set_issue_field_values") => required::<IssueFields<StringOrNumber>>,
        ("issues", "delete_issue_field_value") => no_body,
        ("issues", "add_labels") => optional::<AddLabels>,
        ("issues", "set_labels") => optional::<SetLabels>,
        ("issues", "remove_label") => no_body,
        ("issues", "remove_all_labels") => no_body,
        ("issues", "lock") => optional::<Option<Lock>>,
        ("issues", "unlock") => no_body,
        ("issues", "add_sub_issue") => required::<AddSubIssue>,
        ("issues", "remove_sub_issue") => required::<RemoveSubIssue>,
        ("issues", "reprioritize_sub_issue") => required::<PrioritizeSubIssue>,
        ("issues", "approve_suggestion") => no_body,
        ("issues", "dismiss_suggestion") => no_body,
        ("issues", "create_label") => required::<CreateLabel>,
        ("issues", "update_label") => optional::<UpdateLabel>,
        ("issues", "delete_label") => no_body,
        ("issues", "create_milestone") => required::<CreateMilestone>,
        ("issues", "update_milestone") => optional::<UpdateMilestone>,
        ("issues", "delete_milestone") => no_body,
        ("pulls", "create") => |request| legacy::<CreatePull>(request, true),
        ("pulls", "update") => |request| legacy::<UpdatePull>(request, false),
        ("pulls", "create_review_comment") => required::<ReviewComment>,
        ("pulls", "update_review_comment") => required::<Comment>,
        ("pulls", "delete_review_comment") => no_body,
        ("pulls", "create_reply_for_review_comment") => required::<Comment>,
        ("pulls", "request_reviewers") => optional::<RequestReviewers>,
        ("pulls", "remove_requested_reviewers") => required::<Reviewers>,
        ("pulls", "create_review") => optional::<CreateReview>,
        ("pulls", "update_review") => required::<Comment>,
        ("pulls", "delete_pending_review") => no_body,
        ("pulls", "submit_review") => required::<SubmitReview>,
        _ => return None,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Comment {
    body: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RerunWorkflow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enable_debug_logging: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RerunJob {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enable_debug_logging: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enable_debugger: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrInteger {
    Text(String),
    Integer(i64),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    Text(String),
    Number(f64),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FieldInput {
    Scalar(StringOrNumber),
    Choices(Vec<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Open,
    Closed,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StateReason {
    Completed,
    NotPlanned,
    Duplicate,
    Reopened,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldValue<V> {
    field_id: i64,
    value: V,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "V: Deserialize<'de>"))]
struct IssueFields<V> {
    #[serde(default)]
    issue_field_values: Optional<Vec<FieldValue<V>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestedFieldValue {
    field_id: i64,
    value: FieldInput,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CreateIssueLabel {
    Name(String),
    Details(LabelDetails),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelDetails {
    #[serde(default)]
    id: Optional<i64>,
    #[serde(default)]
    name: Optional<String>,
    #[serde(default)]
    description: Optional<Option<String>>,
    #[serde(default)]
    color: Optional<Option<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UpdateIssueLabel {
    Name(String),
    Details(SuggestedLabelDetails),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuggestedLabelDetails {
    #[serde(default)]
    id: Optional<i64>,
    #[serde(default)]
    name: Optional<String>,
    #[serde(default)]
    description: Optional<Option<String>>,
    #[serde(default)]
    color: Optional<Option<String>>,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateIssue {
    title: StringOrInteger,
    #[serde(default)]
    body: Optional<String>,
    #[serde(default)]
    assignee: Optional<Option<String>>,
    #[serde(default)]
    assignees: Optional<Vec<String>>,
    #[serde(default)]
    milestone: Optional<StringOrInteger>,
    #[serde(default)]
    labels: Optional<Vec<CreateIssueLabel>>,
    #[serde(default)]
    r#type: Optional<Option<String>>,
    #[serde(default)]
    issue_field_values: Optional<Vec<FieldValue<FieldInput>>>,
    #[serde(default)]
    parent_issue_id: Optional<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateIssue {
    #[serde(default)]
    title: Optional<Option<StringOrInteger>>,
    #[serde(default)]
    body: Optional<Option<String>>,
    #[serde(default)]
    assignee: Optional<Option<String>>,
    #[serde(default)]
    assignees: Optional<Vec<UpdateAssignee>>,
    #[serde(default)]
    milestone: Optional<Option<StringOrInteger>>,
    #[serde(default)]
    labels: Optional<Vec<UpdateIssueLabel>>,
    #[serde(default)]
    state: Optional<State>,
    #[serde(default)]
    state_reason: Optional<Option<StateReason>>,
    #[serde(default)]
    r#type: Optional<Option<IssueType>>,
    #[serde(default)]
    issue_field_values: Optional<Vec<SuggestedFieldValue>>,
    #[serde(default)]
    duplicate_issue_id: Optional<i64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IssueType {
    Name(String),
    Suggestion(TypeSuggestion),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeSuggestion {
    #[serde(default)]
    value: Optional<Option<String>>,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AddAssignee {
    Login(String),
    Suggestion(AssigneeSuggestion),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssigneeSuggestion {
    login: String,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum UpdateAssignee {
    Login(String),
    Suggestion(UpdateAssigneeSuggestion),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAssigneeSuggestion {
    #[serde(default)]
    login: Optional<String>,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAssignees {
    #[serde(default)]
    assignees: Optional<Vec<AddAssignee>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveAssignees {
    #[serde(default)]
    assignees: Optional<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    issue_id: i64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AddedLabel {
    Name(String),
    Suggestion(LabelSuggestion),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelSuggestion {
    name: String,
    #[serde(default)]
    suggest: Optional<bool>,
    #[serde(default)]
    rationale: Optional<String>,
    #[serde(default)]
    confidence: Optional<Confidence>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddedLabelsObject {
    #[serde(default)]
    labels: Optional<Vec<AddedLabel>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AddLabels {
    Object(AddedLabelsObject),
    Names(Vec<String>),
    Suggestions(Vec<LabelSuggestion>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelName {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelNames {
    #[serde(default)]
    labels: Optional<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelObjects {
    #[serde(default)]
    labels: Optional<Vec<LabelName>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SetLabels {
    NamesObject(LabelNames),
    Names(Vec<String>),
    LabelsObject(LabelObjects),
    Labels(Vec<LabelName>),
    Name(String),
}

#[derive(Deserialize)]
enum LockReason {
    #[serde(rename = "off-topic")]
    OffTopic,
    #[serde(rename = "too heated")]
    TooHeated,
    #[serde(rename = "resolved")]
    Resolved,
    #[serde(rename = "spam")]
    Spam,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    #[serde(default)]
    lock_reason: Optional<LockReason>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveSubIssue {
    sub_issue_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddSubIssue {
    sub_issue_id: i64,
    #[serde(default)]
    replace_parent: Optional<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrioritizeSubIssue {
    sub_issue_id: i64,
    #[serde(default)]
    after_id: Optional<i64>,
    #[serde(default)]
    before_id: Optional<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLabel {
    name: String,
    #[serde(default)]
    color: Optional<String>,
    #[serde(default)]
    description: Optional<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateLabel {
    #[serde(default)]
    new_name: Optional<String>,
    #[serde(default)]
    color: Optional<String>,
    #[serde(default)]
    description: Optional<String>,
    #[serde(default)]
    archived: Optional<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateMilestone {
    title: String,
    #[serde(default)]
    state: Optional<State>,
    #[serde(default)]
    description: Optional<String>,
    #[serde(default)]
    due_on: Optional<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateMilestone {
    #[serde(default)]
    title: Optional<String>,
    #[serde(default)]
    state: Optional<State>,
    #[serde(default)]
    description: Optional<String>,
    #[serde(default)]
    due_on: Optional<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CreatePull {
    head: String,
    base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head_repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issue: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UpdatePull {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<State>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    maintainer_can_modify: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum ReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

#[derive(Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Side {
    Left,
    Right,
}

#[derive(Deserialize)]
enum StartSide {
    #[serde(rename = "LEFT")]
    Left,
    #[serde(rename = "RIGHT")]
    Right,
    #[serde(rename = "side")]
    Side,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SubjectType {
    Line,
    File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewComment {
    body: String,
    commit_id: String,
    path: String,
    #[serde(default)]
    position: Optional<i64>,
    #[serde(default)]
    line: Optional<i64>,
    #[serde(default)]
    side: Optional<Side>,
    #[serde(default)]
    start_line: Optional<i64>,
    #[serde(default)]
    start_side: Optional<StartSide>,
    #[serde(default)]
    in_reply_to: Optional<i64>,
    #[serde(default)]
    subject_type: Optional<SubjectType>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reviewers {
    reviewers: Vec<String>,
    #[serde(default)]
    team_reviewers: Optional<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TeamReviewers {
    team_reviewers: Vec<String>,
    #[serde(default)]
    reviewers: Optional<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RequestReviewers {
    Users(Reviewers),
    Teams(TeamReviewers),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingReviewComment {
    path: String,
    body: String,
    #[serde(default)]
    line: Optional<i64>,
    #[serde(default)]
    position: Optional<i64>,
    #[serde(default)]
    side: Optional<String>,
    #[serde(default)]
    start_line: Optional<i64>,
    #[serde(default)]
    start_side: Optional<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateReview {
    #[serde(default)]
    commit_id: Optional<String>,
    #[serde(default)]
    body: Optional<String>,
    #[serde(default)]
    event: Optional<ReviewEvent>,
    #[serde(default)]
    comments: Optional<Vec<PendingReviewComment>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitReview {
    event: ReviewEvent,
    #[serde(default)]
    body: Optional<String>,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    use super::*;

    fn valid<T: DeserializeOwned>(value: Value) -> bool {
        serde_json::from_value::<T>(value).is_ok()
    }

    #[test]
    fn omitted_nullable_and_nonnullable_fields_are_distinct() {
        assert!(valid::<UpdateIssue>(json!({})));
        assert!(valid::<UpdateIssue>(
            json!({"body":null,"milestone":null,"title":null,"type":null})
        ));
        assert!(!valid::<UpdateIssue>(json!({"state":null})));
        assert!(!valid::<UpdateIssue>(json!({"assignees":null})));
        assert!(!valid::<CreateIssue>(json!({"title":null})));
        assert!(valid::<UpdatePull>(json!({"body":null})));
        assert!(valid::<Option<RerunJob>>(Value::Null));
        assert!(valid::<RerunJob>(json!({"enable_debugger":null})));
        assert!(!valid::<RerunWorkflow>(json!({"enable_debugger":true})));
    }

    #[test]
    fn reviewer_and_assignee_requirements_depend_on_the_write() {
        for value in [
            json!({"reviewers":["alice"]}),
            json!({"team_reviewers":["maintainers"]}),
        ] {
            assert!(valid::<RequestReviewers>(value));
        }
        assert!(!valid::<RequestReviewers>(json!({})));
        assert!(!valid::<RequestReviewers>(json!({"reviewers":null})));
        assert!(!valid::<Reviewers>(
            json!({"team_reviewers":["maintainers"]})
        ));
        assert!(valid::<AddAssignees>(
            json!({"assignees":[{"login":"alice","confidence":"high"}]})
        ));
        assert!(!valid::<AddAssignees>(
            json!({"assignees":[{"suggest":true}]})
        ));
        assert!(valid::<UpdateIssue>(
            json!({"assignees":[{"suggest":true}]})
        ));
        assert!(!valid::<RemoveAssignees>(
            json!({"assignees":[{"login":"alice"}]})
        ));
    }

    #[test]
    fn label_and_field_value_unions_are_not_interchangeable() {
        assert!(valid::<AddLabels>(
            json!({"labels":["UI",{"name":"bug","confidence":"medium"}]})
        ));
        assert!(valid::<AddLabels>(json!([{"name":"bug","suggest":true}])));
        assert!(!valid::<AddLabels>(json!(["UI",{"name":"bug"}])));
        assert!(!valid::<SetLabels>(
            json!({"labels":[{"name":"bug","suggest":true}]})
        ));
        assert!(valid::<SetLabels>(json!("bug")));
        assert!(!valid::<AddLabels>(json!("bug")));
        assert!(valid::<IssueFields<FieldInput>>(
            json!({"issue_field_values":[{"field_id":5,"value":["High","UI"]}]})
        ));
        assert!(!valid::<IssueFields<StringOrNumber>>(
            json!({"issue_field_values":[{"field_id":5,"value":["High","UI"]}]})
        ));
        assert!(!valid::<IssueFields<FieldInput>>(
            json!({"issue_field_values":[{"value":"High"}]})
        ));
        assert!(!valid::<IssueFields<FieldInput>>(
            json!({"issue_field_values":[{"field_id":5,"value":true}]})
        ));
    }

    #[test]
    fn nested_review_types_and_enum_spellings_match_github() {
        assert!(valid::<SubmitReview>(json!({"event":"REQUEST_CHANGES"})));
        assert!(!valid::<SubmitReview>(json!({"event":"REQUESTCHANGES"})));
        assert!(!valid::<SubmitReview>(json!({"body":"missing verdict"})));
        assert!(valid::<ReviewComment>(
            json!({"body":"check","commit_id":"abc","path":"foo","side":"RIGHT","start_side":"LEFT"})
        ));
        assert!(!valid::<ReviewComment>(
            json!({"body":"check","commit_id":"abc","path":"foo","side":"right"})
        ));
        assert!(!valid::<CreateReview>(
            json!({"comments":[{"path":"foo","line":5}]})
        ));
        assert!(!valid::<CreateReview>(
            json!({"comments":[{"path":"foo","body":"check","line":"five"}]})
        ));
        assert!(valid::<Lock>(json!({"lock_reason":"too heated"})));
        assert!(!valid::<Lock>(json!({"lock_reason":"too_heated"})));
    }
}
