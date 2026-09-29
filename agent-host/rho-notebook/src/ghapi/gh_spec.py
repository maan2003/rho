"""Selected entries from ghapi 2.1.5 GitHub OpenAPI metadata (Apache-2.0)."""
spec = {'base_url': 'https://api.github.com',
 'ops': [{'group': 'checks',
          'name': 'list_for_ref',
          'path': '/repos/{owner}/{repo}/commits/{ref}/check-runs',
          'verb': 'GET',
          'summary': 'List check runs for a Git reference',
          'route_params': ['owner', 'repo', 'ref'],
          'query_params': ['per_page', 'page'],
          'required_params': ['owner', 'ref', 'repo'],
          'param_types': {'owner': 'str', 'repo': 'str', 'ref': 'str', 'per_page': 'int', 'page': 'int'},
          'param_defaults': {'per_page': 30, 'page': 1},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'ref': 'Octo accepts a 40-character commit SHA.',
                         'per_page': 'The number of results per page (max 100). For more information, '
                                     'see "[Using pagination in the REST '
                                     'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."',
                         'page': 'The page number of the results to fetch. For more information, see '
                                 '"[Using pagination in the REST '
                                 'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."'},
          'docs_url': 'https://docs.github.com/rest/checks/runs#list-check-runs-for-a-git-reference'},
         {'group': 'repos',
          'name': 'get_combined_status_for_ref',
          'path': '/repos/{owner}/{repo}/commits/{ref}/status',
          'verb': 'GET',
          'summary': 'Get the combined status for a specific reference',
          'route_params': ['owner', 'repo', 'ref'],
          'query_params': [],
          'required_params': ['owner', 'ref', 'repo'],
          'param_types': {'owner': 'str', 'repo': 'str', 'ref': 'str'},
          'param_defaults': {},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'ref': 'Octo accepts a 40-character commit SHA.'},
          'docs_url': 'https://docs.github.com/rest/commits/statuses#get-the-combined-status-for-a-specific-reference'},
         {'group': 'issues',
          'name': 'list_for_repo',
          'path': '/repos/{owner}/{repo}/issues',
          'verb': 'GET',
          'summary': 'List repository issues',
          'route_params': ['owner', 'repo'],
          'query_params': ['state', 'per_page', 'page'],
          'required_params': ['owner', 'repo'],
          'param_types': {'owner': 'str',
                          'repo': 'str',
                          'state': 'str',
                          'per_page': 'int',
                          'page': 'int'},
          'param_defaults': {'state': 'open', 'per_page': 30, 'page': 1},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'state': 'Indicates the state of the issues to return.',
                         'per_page': 'The number of results per page (max 100). For more information, '
                                     'see "[Using pagination in the REST '
                                     'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."',
                         'page': 'The page number of the results to fetch. For more information, see '
                                 '"[Using pagination in the REST '
                                 'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."'},
          'docs_url': 'https://docs.github.com/rest/issues/issues#list-repository-issues'},
         {'group': 'issues',
          'name': 'get',
          'path': '/repos/{owner}/{repo}/issues/{issue_number}',
          'verb': 'GET',
          'summary': 'Get an issue',
          'route_params': ['owner', 'repo', 'issue_number'],
          'required_params': ['issue_number', 'owner', 'repo'],
          'param_types': {'owner': 'str', 'repo': 'str', 'issue_number': 'int'},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'issue_number': 'The number that identifies the issue.'},
          'docs_url': 'https://docs.github.com/rest/issues/issues#get-an-issue'},
         {'group': 'pulls',
          'name': 'list',
          'path': '/repos/{owner}/{repo}/pulls',
          'verb': 'GET',
          'summary': 'List pull requests',
          'route_params': ['owner', 'repo'],
          'query_params': ['state', 'per_page', 'page'],
          'required_params': ['owner', 'repo'],
          'param_types': {'owner': 'str',
                          'repo': 'str',
                          'state': 'str',
                          'per_page': 'int',
                          'page': 'int'},
          'param_defaults': {'state': 'open', 'per_page': 30, 'page': 1},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'state': 'Either `open`, `closed`, or `all` to filter by state.',
                         'per_page': 'The number of results per page (max 100). For more information, '
                                     'see "[Using pagination in the REST '
                                     'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."',
                         'page': 'The page number of the results to fetch. For more information, see '
                                 '"[Using pagination in the REST '
                                 'API](https://docs.github.com/rest/using-the-rest-api/using-pagination-in-the-rest-api)."'},
          'docs_url': 'https://docs.github.com/rest/pulls/pulls#list-pull-requests'},
         {'group': 'pulls',
          'name': 'create',
          'path': '/repos/{owner}/{repo}/pulls',
          'verb': 'POST',
          'summary': 'Create a pull request',
          'route_params': ['owner', 'repo'],
          'body_params': ['title', 'head', 'base', 'body', 'maintainer_can_modify', 'draft'],
          'request_content_type': 'application/json',
          'required_params': ['base', 'head', 'owner', 'repo'],
          'param_types': {'owner': 'str',
                          'repo': 'str',
                          'title': 'str',
                          'head': 'str',
                          'base': 'str',
                          'body': 'str',
                          'maintainer_can_modify': 'bool',
                          'draft': 'bool'},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'title': 'The title of the new pull request. Required unless `issue` is '
                                  'specified.',
                         'head': 'The name of the branch where your changes are implemented. For '
                                 'cross-repository pull requests in the same network, namespace `head` '
                                 'with a user like this: `username:branch`.',
                         'base': 'The name of the branch you want the changes pulled into. This should '
                                 'be an existing branch on the current repository. You cannot submit a '
                                 'pull request to one repository that requests a merge to a base of '
                                 'another repository.',
                         'body': 'The contents of the pull request.',
                         'maintainer_can_modify': 'Indicates whether [maintainers can '
                                                  'modify](https://docs.github.com/articles/allowing-changes-to-a-pull-request-branch-created-from-a-fork/) '
                                                  'the pull request.',
                         'draft': 'Indicates whether the pull request is a draft. See "[Draft Pull '
                                  'Requests](https://docs.github.com/articles/about-pull-requests#draft-pull-requests)" '
                                  'in the GitHub Help documentation to learn more.'},
          'body_examples': {'default': {'summary': '',
                                        'value': {'title': 'Amazing new feature',
                                                  'body': 'Please pull these awesome changes in!',
                                                  'head': 'octocat:new-feature',
                                                  'base': 'master'}}},
          'docs_url': 'https://docs.github.com/rest/pulls/pulls#create-a-pull-request'},
         {'group': 'pulls',
          'name': 'get',
          'path': '/repos/{owner}/{repo}/pulls/{pull_number}',
          'verb': 'GET',
          'summary': 'Get a pull request',
          'route_params': ['owner', 'repo', 'pull_number'],
          'required_params': ['owner', 'pull_number', 'repo'],
          'param_types': {'owner': 'str', 'repo': 'str', 'pull_number': 'int'},
          'param_docs': {'owner': 'The account owner of the repository. The name is not case sensitive.',
                         'repo': 'The name of the repository without the `.git` extension. The name is '
                                 'not case sensitive.',
                         'pull_number': 'The number that identifies the pull request.'},
          'docs_url': 'https://docs.github.com/rest/pulls/pulls#get-a-pull-request'}]}
