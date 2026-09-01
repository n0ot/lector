local pager = {}

local function has_pager_prompt(view)
    local line = view:line(view.rows - 1)
    local cursor = view.cursor
    return (cursor ~= nil and cursor.visible and cursor.row == view.rows - 1) or
        line == ":" or line:match("%(END%)$") ~= nil
end

local function page_end(view)
    if has_pager_prompt(view) then
        return {row = view.rows - 1, col = 0}
    end
    return view:bottom()
end

local function before(left, right)
    return left.row < right.row or
        (left.row == right.row and left.col < right.col)
end

function pager.read()
    local view = lector.api.view()
    local reader = lector.api.reader()
    local first_page = true
    local stop_after_page = false

    while true do
        local first = first_page and view.review or view:top()
        local last = page_end(view)
        local result = {status = "completed", position = first}
        if before(first, last) then
            result = reader:read(view, first, last)
        end
        if result.status ~= "completed" then
            return
        end

        if stop_after_page then
            reader:close()
            return
        end

        local response = view:send_keys("<C-f>"):wait_for_stable_screen()
        if response.status ~= "presented" or not response.content_changed then
            reader:close()
            return
        end

        view = response.view
        stop_after_page = response.effects.bells > 0
        first_page = false
    end
end

return pager
